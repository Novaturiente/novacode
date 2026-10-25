//! Project trust (L6.3): whether a folder's executable config (project
//! settings, extensions, MCP, hooks, project skills) may load. Decisions live
//! in `<home>/trust.json`; the nearest ancestor entry wins. AGENTS.md and
//! CLAUDE.md are plain text and always load (L4.9 revision).
//!
//! A trusted entry also keeps a fingerprint of the project config it saw, per
//! folder it was used from (`"."` = the entry's own folder). When the config
//! changes after that, the folder counts as changed (like direnv's `allow`):
//! the binary asks again, or ignores the config when nobody can answer.

use novacode_base::storage::{self, Durability};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::{self, Read};
use std::path::{Component, Path, PathBuf};

pub struct TrustStore {
    path: PathBuf,
}

/// Folders a project can configure novacode from; trust is only asked when one exists.
const PROJECT_CONFIG: &[&str] = &[
    ".novacode/settings.json",
    ".novacode/extensions",
    ".novacode/skills",
    ".novacode/agents",
    ".novacode/packages",
    ".novacode/packages.json",
    ".novacode/packages.lock.json",
    ".mcp.json",
    ".claude/skills",
    ".claude/agents",
    ".claude/commands",
];

/// Paths hashed per config entry at most.
const MAX_PATHS: usize = 4096;
/// File bytes hashed per fingerprint at most.
const MAX_BYTES: u64 = 256 << 20;
/// The change reported when a cap is hit: the config can't be checked, so it counts as changed.
const TOO_MANY: &str = "too many files to check";
/// The change reported for config a trusted entry never saw from this folder.
const NEW: &str = "new project config";

/// Per config entry that exists (`PROJECT_CONFIG` names, and `../.mcp.json` style
/// keys for parent folders' `.mcp.json`): FNV-1a 64 of its paths and contents, as hex.
pub type Fingerprint = BTreeMap<String, String>;

/// One `trust.json` entry. Old files store a bare `true`/`false`.
#[derive(Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(from = "Stored")]
struct Decision {
    trusted: bool,
    /// Keyed by the folder it was taken in, relative to this entry (`"."` = itself).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    fingerprints: BTreeMap<String, Fingerprint>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Stored {
    Old(bool),
    New {
        trusted: bool,
        #[serde(default)]
        fingerprints: BTreeMap<String, Fingerprint>,
    },
}

impl From<Stored> for Decision {
    fn from(s: Stored) -> Self {
        match s {
            Stored::Old(trusted) => Decision {
                trusted,
                fingerprints: BTreeMap::new(),
            },
            Stored::New { trusted, fingerprints } => Decision { trusted, fingerprints },
        }
    }
}

/// Where `dir` stands for loading its project config.
#[derive(Debug, PartialEq)]
pub enum State {
    /// No entry at the folder or any parent.
    Unknown,
    Untrusted,
    Trusted,
    /// Trusted, but the project config changed since: what changed, e.g. "added .mcp.json".
    Changed(Vec<String>),
}

impl TrustStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    fn read(&self) -> BTreeMap<PathBuf, Decision> {
        match storage::read_json(&self.path) {
            Ok(Some(m)) => m,
            Ok(None) => BTreeMap::new(),
            Err(e) => {
                tracing::warn!(path = %self.path.display(), error = %e, "unreadable trust file; treating every project as untrusted");
                BTreeMap::new()
            }
        }
    }

    fn update(&self, f: impl FnOnce(&mut BTreeMap<PathBuf, Decision>)) -> io::Result<()> {
        let _lock = storage::lock(&self.path)?;
        let mut map = self.read();
        f(&mut map);
        let bytes = serde_json::to_vec_pretty(&map).map_err(io::Error::other)?;
        storage::write_atomic(&self.path, &bytes, Durability::Durable)
    }

    /// The decision for `dir`: its own entry or its nearest ancestor's. `None` = never asked.
    pub fn decision(&self, dir: &Path) -> Option<bool> {
        let map = self.read();
        dir.ancestors().find_map(|a| map.get(a).map(|d| d.trusted))
    }

    /// The decision for `dir` with its fingerprint checked.
    pub fn state(&self, dir: &Path) -> State {
        self.check(dir).0
    }

    /// The decision for `dir` with its fingerprint checked, and that fingerprint (to
    /// store with [`TrustStore::trust_with`] when the user trusts what was shown).
    /// A trusted entry with no fingerprint for `dir` yet takes the current one silently
    /// when the entry has none at all (old format) or every config file `dir` reads is
    /// one the entry already saw unchanged; otherwise `dir` counts as changed.
    pub fn check(&self, dir: &Path) -> (State, Fingerprint) {
        let (now, complete) = scan(dir);
        let map = self.read();
        let Some((at, d)) = dir.ancestors().find_map(|a| map.get(a).map(|d| (a, d))) else {
            return (State::Unknown, now);
        };
        if !d.trusted {
            return (State::Untrusted, now);
        }
        if !complete {
            return (State::Changed(vec![TOO_MANY.into()]), now);
        }
        let key = rel_key(at, dir);
        let state = match d.fingerprints.get(&key) {
            Some(old) if *old == now => State::Trusted,
            Some(old) => State::Changed(changes(old, &now)),
            None if !d.fingerprints.is_empty() && !all_seen(at, d, dir, &now) => State::Changed(vec![NEW.into()]),
            None => {
                let at = at.to_path_buf();
                let fp = now.clone();
                let r = self.update(|m| {
                    if let Some(d) = m.get_mut(&at) {
                        d.fingerprints.insert(key, fp);
                    }
                });
                if let Err(e) = r {
                    tracing::warn!(path = %self.path.display(), error = %e, "couldn't store the project config fingerprint");
                }
                State::Trusted
            }
        };
        (state, now)
    }

    /// Record a decision for `dir` (and everything below it); trusting stores its current fingerprint.
    pub fn set(&self, dir: &Path, trusted: bool) -> io::Result<()> {
        self.set_from(dir, trusted, dir)
    }

    /// Record a decision for `at`, taken in `cwd` (itself or a folder below it):
    /// trusting stores `cwd`'s current fingerprint.
    pub fn set_from(&self, at: &Path, trusted: bool, cwd: &Path) -> io::Result<()> {
        let fp = trusted.then(|| fingerprint(cwd));
        self.store(at, trusted, cwd, fp)
    }

    /// Trust `at`, taken in `cwd`, storing `fp`: the fingerprint the user was shown
    /// ([`TrustStore::check`]), not one taken after the answer.
    pub fn trust_with(&self, at: &Path, cwd: &Path, fp: Fingerprint) -> io::Result<()> {
        self.store(at, true, cwd, Some(fp))
    }

    /// After a change the user agreed to in a folder that was [`State::Trusted`] just before
    /// (installing its declared packages), keep it trusted: store `dir`'s new fingerprint.
    pub fn accept_current(&self, dir: &Path) -> io::Result<()> {
        let map = self.read();
        let Some(at) = dir.ancestors().find(|a| map.get(*a).is_some_and(|d| d.trusted)) else {
            return Ok(());
        };
        self.store(at, true, dir, Some(fingerprint(dir)))
    }

    fn store(&self, at: &Path, trusted: bool, cwd: &Path, fp: Option<Fingerprint>) -> io::Result<()> {
        let fp = fp.filter(|_| cwd.starts_with(at)).map(|f| (rel_key(at, cwd), f));
        self.update(|m| {
            let d = m.entry(at.to_path_buf()).or_default();
            d.trusted = trusted;
            if let Some((k, f)) = fp {
                d.fingerprints.insert(k, f);
            }
        })
    }

    pub fn remove(&self, dir: &Path) -> io::Result<bool> {
        let mut had = false;
        self.update(|m| had = m.remove(dir).is_some())?;
        Ok(had)
    }
}

fn rel_key(at: &Path, dir: &Path) -> String {
    match dir.strip_prefix(at) {
        Ok(r) if !r.as_os_str().is_empty() => r.to_string_lossy().into_owned(),
        _ => ".".into(),
    }
}

/// `p` with `.` and `..` removed without touching the disk.
fn lexical(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            c => out.push(c),
        }
    }
    out
}

/// True when every config file in `now` (taken in `dir`) is one entry `d` (at `at`)
/// already stored a fingerprint for, with the same hash: e.g. a parent's `.mcp.json`
/// trusted from the parent.
fn all_seen(at: &Path, d: &Decision, dir: &Path, now: &Fingerprint) -> bool {
    let seen: BTreeMap<PathBuf, &String> = d
        .fingerprints
        .iter()
        .flat_map(|(folder, fp)| fp.iter().map(move |(k, h)| (lexical(&at.join(folder).join(k)), h)))
        .collect();
    now.iter().all(|(k, h)| seen.get(&lexical(&dir.join(k))) == Some(&h))
}

/// What differs between two fingerprints, one line per config entry.
fn changes(old: &Fingerprint, now: &Fingerprint) -> Vec<String> {
    let mut names: Vec<&String> = old.keys().chain(now.keys()).collect();
    names.sort();
    names.dedup();
    names
        .into_iter()
        .filter_map(|name| match (old.get(name), now.get(name)) {
            (None, Some(_)) => Some(format!("added {name}")),
            (Some(_), None) => Some(format!("removed {name}")),
            (Some(a), Some(b)) if a != b => Some(format!("changed {name}")),
            _ => None,
        })
        .collect()
}

/// The config files `dir` loads that exist, as (fingerprint key, path, folder it sits
/// in): its own `PROJECT_CONFIG` entries, then the `.mcp.json` of each parent folder
/// the MCP loader reads ([`crate::mcp::project_dirs`]), keyed `../.mcp.json`, `../../.mcp.json`.
fn entries(dir: &Path) -> Vec<(String, PathBuf, &Path)> {
    let mut v: Vec<_> = PROJECT_CONFIG
        .iter()
        .map(|n| (n.to_string(), dir.join(n), dir))
        .collect();
    for (up, a) in crate::mcp::project_dirs(dir).into_iter().enumerate().skip(1) {
        v.push(("../".repeat(up) + ".mcp.json", a.join(".mcp.json"), a));
    }
    v.retain(|(_, p, _)| p.exists());
    v
}

/// True when `dir` has project config that trust would gate (its parents' `.mcp.json` included).
pub fn has_project_config(dir: &Path) -> bool {
    !entries(dir).is_empty()
}

/// Resolve trust without a UI: `--trust` wins, then a stored decision whose
/// fingerprint still matches; otherwise untrusted.
/// The interactive 4-choice prompt lives in the binary (it needs the TUI).
pub fn resolve(store: &TrustStore, dir: &Path, cli_trust: bool) -> bool {
    cli_trust || store.state(dir) == State::Trusted
}

/// The fingerprint of `dir`'s project config: each config entry that exists
/// ([`entries`]), hashed over its sorted relative paths and file contents. Missing
/// entries are left out, so creating one changes the fingerprint.
pub fn fingerprint(dir: &Path) -> Fingerprint {
    scan(dir).0
}

/// [`fingerprint`], and false when a cap was hit (the fingerprint doesn't cover
/// everything): `MAX_PATHS` paths per entry or `MAX_BYTES` file bytes in total.
fn scan(dir: &Path) -> (Fingerprint, bool) {
    scan_capped(dir, MAX_BYTES)
}

fn scan_capped(dir: &Path, max_bytes: u64) -> (Fingerprint, bool) {
    let mut bytes = max_bytes;
    let mut complete = true;
    let fp = entries(dir)
        .into_iter()
        .map(|(key, path, root)| {
            let mut h = Fnv::new();
            let mut paths = MAX_PATHS;
            complete &= hash_path(&mut h, root, &path, &mut paths, &mut bytes);
            (key, format!("{:016x}", h.0))
        })
        .collect();
    (fp, complete)
}

/// Hash `path` (relative to `root`) and everything below it; false when a budget ran out.
/// `.git` folders are skipped: nothing loads from them, and git rewrites them on its own.
fn hash_path(h: &mut Fnv, root: &Path, path: &Path, paths: &mut usize, bytes: &mut u64) -> bool {
    if *paths == 0 {
        return false;
    }
    *paths -= 1;
    h.write(path.strip_prefix(root).unwrap_or(path).to_string_lossy().as_bytes());
    h.write(b"\0");
    if let Ok(target) = std::fs::read_link(path) {
        h.write(b"->");
        h.write(target.to_string_lossy().as_bytes());
        h.write(b"\0");
    }
    let Ok(meta) = std::fs::metadata(path) else { return true };
    if meta.is_dir() {
        let mut kids: Vec<PathBuf> = std::fs::read_dir(path)
            .map(|rd| {
                rd.flatten()
                    .map(|e| e.path())
                    .filter(|p| !p.ends_with(".git"))
                    .collect()
            })
            .unwrap_or_default();
        kids.sort();
        kids.iter().all(|k| hash_path(h, root, k, paths, bytes))
    } else if let Ok(mut f) = std::fs::File::open(path) {
        let mut buf = vec![0; 64 << 10];
        let mut len = 0u64;
        loop {
            match f.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if n as u64 > *bytes {
                        return false;
                    }
                    *bytes -= n as u64;
                    len += n as u64;
                    h.write(&buf[..n]);
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => break,
            }
        }
        h.write(&len.to_le_bytes());
        true
    } else {
        true
    }
}

/// FNV-1a 64: stable across runs and versions (std's hasher is randomised).
struct Fnv(u64);

impl Fnv {
    fn new() -> Self {
        Fnv(0xcbf2_9ce4_8422_2325)
    }
    fn write(&mut self, bytes: &[u8]) {
        for b in bytes {
            self.0 = (self.0 ^ *b as u64).wrapping_mul(0x0100_0000_01b3);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nearest_ancestor_wins() {
        let d = tempfile::tempdir().unwrap();
        let s = TrustStore::new(d.path().join("trust.json"));
        assert_eq!(s.decision(Path::new("/w/a/b")), None);
        s.set(Path::new("/w"), true).unwrap();
        s.set(Path::new("/w/a"), false).unwrap();
        assert_eq!(s.decision(Path::new("/w/x")), Some(true));
        assert_eq!(s.decision(Path::new("/w/a/b")), Some(false));
        assert!(!resolve(&s, Path::new("/w/a/b"), false));
        assert!(resolve(&s, Path::new("/w/a/b"), true));
        assert!(s.remove(Path::new("/w/a")).unwrap());
        assert_eq!(s.decision(Path::new("/w/a/b")), Some(true));
    }

    #[test]
    fn old_format_reads_and_adopts_a_fingerprint() {
        let home = tempfile::tempdir().unwrap();
        let proj = tempfile::tempdir().unwrap();
        std::fs::write(proj.path().join(".mcp.json"), "{}").unwrap();
        let file = home.path().join("trust.json");
        let p = proj.path().display().to_string();
        std::fs::write(
            &file,
            serde_json::json!({ p.clone(): true, "/nope": false }).to_string(),
        )
        .unwrap();
        let s = TrustStore::new(&file);
        assert_eq!(s.decision(Path::new("/nope/x")), Some(false));
        assert_eq!(s.state(Path::new("/nope/x")), State::Untrusted);
        // No fingerprint yet: taken silently, then checked.
        assert_eq!(s.state(proj.path()), State::Trusted);
        let stored: serde_json::Value = serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
        assert_eq!(stored["/nope"], serde_json::json!({ "trusted": false }));
        assert!(stored[&p]["fingerprints"]["."][".mcp.json"].is_string());
        std::fs::write(proj.path().join(".mcp.json"), "{\"x\":1}").unwrap();
        assert_eq!(s.state(proj.path()), State::Changed(vec!["changed .mcp.json".into()]));
    }

    #[test]
    fn fingerprint_tracks_project_config_only() {
        let d = tempfile::tempdir().unwrap();
        let dir = d.path();
        std::fs::create_dir_all(dir.join(".novacode/extensions")).unwrap();
        std::fs::write(dir.join(".novacode/extensions/a.ts"), "one").unwrap();
        let base = fingerprint(dir);

        // Unrelated files don't count.
        std::fs::write(dir.join("README.md"), "hi").unwrap();
        std::fs::write(dir.join("AGENTS.md"), "hi").unwrap();
        std::fs::create_dir_all(dir.join(".novacode/prompts")).unwrap();
        std::fs::write(dir.join(".novacode/prompts/p.md"), "hi").unwrap();
        assert_eq!(fingerprint(dir), base);

        // A new file under a config folder.
        std::fs::write(dir.join(".novacode/extensions/b.ts"), "two").unwrap();
        let added = fingerprint(dir);
        assert_ne!(added, base);
        assert_eq!(changes(&base, &added), ["changed .novacode/extensions"]);

        // A file's content.
        std::fs::write(dir.join(".novacode/extensions/a.ts"), "uno").unwrap();
        let edited = fingerprint(dir);
        assert_ne!(edited, added);

        // A new top-level entry.
        std::fs::write(dir.join(".mcp.json"), "{}").unwrap();
        let mcp = fingerprint(dir);
        assert_eq!(changes(&edited, &mcp), ["added .mcp.json"]);
        std::fs::remove_file(dir.join(".mcp.json")).unwrap();
        assert_eq!(changes(&mcp, &fingerprint(dir)), ["removed .mcp.json"]);
        assert_eq!(fingerprint(dir), edited);
    }

    #[test]
    fn stored_fingerprint_round_trips() {
        let home = tempfile::tempdir().unwrap();
        let w = tempfile::tempdir().unwrap();
        let sub = w.path().join("sub");
        std::fs::create_dir_all(sub.join(".claude/skills")).unwrap();
        std::fs::write(sub.join(".claude/skills/s.md"), "x").unwrap();
        let s = TrustStore::new(home.path().join("trust.json"));
        assert_eq!(s.state(&sub), State::Unknown);
        // Trusting the parent from `sub` stores sub's fingerprint on the parent entry.
        s.set_from(w.path(), true, &sub).unwrap();
        assert_eq!(s.state(&sub), State::Trusted);
        assert_eq!(s.state(&sub), State::Trusted);
        std::fs::write(sub.join(".claude/skills/s.md"), "y").unwrap();
        assert_eq!(s.state(&sub), State::Changed(vec!["changed .claude/skills".into()]));
        assert!(!resolve(&s, &sub, false));
        assert!(resolve(&s, &sub, true));
        // Trusting again stores the new fingerprint.
        s.set_from(w.path(), true, &sub).unwrap();
        assert_eq!(s.state(&sub), State::Trusted);
        // Revoking keeps working as before.
        s.set(&sub, false).unwrap();
        assert_eq!(s.state(&sub), State::Untrusted);
    }

    fn write(p: &Path, text: &str) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, text).unwrap();
    }

    #[test]
    fn packages_count_as_project_config() {
        let home = tempfile::tempdir().unwrap();
        let w = tempfile::tempdir().unwrap();
        let dir = w.path();
        let s = TrustStore::new(home.path().join("trust.json"));
        s.set(dir, true).unwrap();
        for (file, change) in [
            (".novacode/packages/evil/hooks/hooks.json", "added .novacode/packages"),
            (".novacode/packages.json", "added .novacode/packages.json"),
            (".novacode/packages.lock.json", "added .novacode/packages.lock.json"),
        ] {
            write(&dir.join(file), "{}");
            assert_eq!(s.state(dir), State::Changed(vec![change.into()]), "{file}");
            s.set(dir, true).unwrap();
        }
        // A package's git metadata isn't loaded, so it doesn't count.
        write(&dir.join(".novacode/packages/evil/.git/FETCH_HEAD"), "x");
        assert_eq!(s.state(dir), State::Trusted);
    }

    #[test]
    fn parent_mcp_json_up_to_the_git_top_counts() {
        let home = tempfile::tempdir().unwrap();
        let w = tempfile::tempdir().unwrap();
        let proj = w.path().join("proj");
        let src = proj.join("src");
        std::fs::create_dir_all(proj.join(".git")).unwrap();
        std::fs::create_dir_all(&src).unwrap();
        let s = TrustStore::new(home.path().join("trust.json"));
        s.set(&proj, true).unwrap();
        // Above the git top level: never read, so never asked about.
        write(&w.path().join(".mcp.json"), "{}");
        assert!(!has_project_config(&src));
        // Planted in the project top while working in `src`.
        write(&proj.join(".mcp.json"), "{}");
        assert!(has_project_config(&src));
        assert_eq!(s.state(&src), State::Changed(vec![NEW.into()]));
        assert_eq!(s.state(&proj), State::Changed(vec!["added .mcp.json".into()]));
        // Trusted from the top: `src` sees the same file unchanged and takes it silently.
        s.set(&proj, true).unwrap();
        assert_eq!(s.state(&src), State::Trusted);
        write(&proj.join(".mcp.json"), "{\"x\":1}");
        assert_eq!(s.state(&src), State::Changed(vec!["changed ../.mcp.json".into()]));
    }

    #[test]
    fn new_config_below_a_trusted_entry_asks() {
        let home = tempfile::tempdir().unwrap();
        let w = tempfile::tempdir().unwrap();
        let x = w.path().join("crates/x");
        std::fs::create_dir_all(&x).unwrap();
        write(&w.path().join(".novacode/settings.json"), "{}");
        let s = TrustStore::new(home.path().join("trust.json"));
        s.set(w.path(), true).unwrap();
        // No config of its own: nothing to ask about.
        assert_eq!(s.state(&x), State::Trusted);
        let y = w.path().join("crates/y");
        write(&y.join(".novacode/extensions/evil.ts"), "evil");
        assert_eq!(s.state(&y), State::Changed(vec![NEW.into()]));
        assert_eq!(s.state(&y), State::Changed(vec![NEW.into()]), "not adopted");
        s.set_from(w.path(), true, &y).unwrap();
        assert_eq!(s.state(&y), State::Trusted);
    }

    #[test]
    fn caps_fail_closed_and_large_files_hash_by_content() {
        let home = tempfile::tempdir().unwrap();
        let w = tempfile::tempdir().unwrap();
        let dir = w.path();
        let skills = dir.join(".novacode/skills");
        std::fs::create_dir_all(&skills).unwrap();
        for i in 0..MAX_PATHS {
            std::fs::write(skills.join(format!("{i}.md")), "").unwrap();
        }
        write(&dir.join(".mcp.json"), "{}");
        // One entry over its path budget doesn't starve the others.
        let before = fingerprint(dir);
        write(&dir.join(".mcp.json"), "{\"x\":1}");
        assert_eq!(changes(&before, &fingerprint(dir)), ["changed .mcp.json"]);
        // But the state fails closed, even right after trusting.
        let s = TrustStore::new(home.path().join("trust.json"));
        s.set(dir, true).unwrap();
        assert_eq!(s.state(dir), State::Changed(vec![TOO_MANY.into()]));
        std::fs::remove_dir_all(&skills).unwrap();
        s.set(dir, true).unwrap();
        assert_eq!(s.state(dir), State::Trusted);

        // A same-size swap of a large file with its mtime put back is seen.
        let big = dir.join(".novacode/extensions/big.js");
        write(&big, &"a".repeat(3 << 20));
        let a = fingerprint(dir);
        let mtime = std::fs::metadata(&big).unwrap().modified().unwrap();
        write(&big, &("b".to_string() + &"a".repeat((3 << 20) - 1)));
        std::fs::File::options()
            .write(true)
            .open(&big)
            .unwrap()
            .set_modified(mtime)
            .unwrap();
        assert_ne!(fingerprint(dir), a);
        // The byte cap.
        assert!(scan_capped(dir, 4 << 20).1);
        assert!(!scan_capped(dir, 1 << 20).1);
    }

    #[test]
    fn trust_with_stores_the_fingerprint_shown() {
        let home = tempfile::tempdir().unwrap();
        let w = tempfile::tempdir().unwrap();
        let dir = w.path();
        write(&dir.join(".mcp.json"), "{}");
        let s = TrustStore::new(home.path().join("trust.json"));
        let (state, shown) = s.check(dir);
        assert_eq!(state, State::Unknown);
        // Changed while the prompt was open: the answer covers only what was shown.
        write(&dir.join(".mcp.json"), "{\"x\":1}");
        s.trust_with(dir, dir, shown).unwrap();
        assert_eq!(s.state(dir), State::Changed(vec!["changed .mcp.json".into()]));
    }

    #[test]
    fn accepting_an_agreed_change_keeps_trust() {
        let home = tempfile::tempdir().unwrap();
        let w = tempfile::tempdir().unwrap();
        let dir = w.path();
        write(&dir.join(".novacode/packages.json"), "[]");
        let s = TrustStore::new(home.path().join("trust.json"));
        // Undecided: accepting does nothing.
        s.accept_current(dir).unwrap();
        assert_eq!(s.state(dir), State::Unknown);
        s.set(dir, true).unwrap();
        // Installing the declared packages changes the fingerprint…
        write(&dir.join(".novacode/packages/p/x.md"), "x");
        assert!(matches!(s.state(dir), State::Changed(_)));
        // …and accepting that install keeps the folder trusted.
        s.accept_current(dir).unwrap();
        assert_eq!(s.state(dir), State::Trusted);
    }
}
