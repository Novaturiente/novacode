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
use std::io;
use std::path::{Path, PathBuf};

pub struct TrustStore {
    path: PathBuf,
}

/// Folders a project can configure novacode from; trust is only asked when one exists.
const PROJECT_CONFIG: &[&str] = &[
    ".novacode/settings.json",
    ".novacode/extensions",
    ".novacode/skills",
    ".novacode/agents",
    ".mcp.json",
    ".claude/skills",
    ".claude/agents",
    ".claude/commands",
];

/// Files above this size are hashed by size and mtime, not content.
const MAX_FILE: u64 = 1 << 20;
/// Paths hashed per fingerprint at most; the rest are cut off.
const MAX_PATHS: usize = 4096;

/// Per `PROJECT_CONFIG` entry that exists: FNV-1a 64 of its paths and contents, as hex.
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

    /// The decision for `dir` with its fingerprint checked. A trusted entry with no
    /// fingerprint for `dir` yet (first start after upgrade) takes the current one silently.
    pub fn state(&self, dir: &Path) -> State {
        let map = self.read();
        let Some((at, d)) = dir.ancestors().find_map(|a| map.get(a).map(|d| (a, d))) else {
            return State::Unknown;
        };
        if !d.trusted {
            return State::Untrusted;
        }
        let key = rel_key(at, dir);
        let now = fingerprint(dir);
        match d.fingerprints.get(&key) {
            Some(old) if *old == now => State::Trusted,
            Some(old) => State::Changed(changes(old, &now)),
            None => {
                let at = at.to_path_buf();
                let r = self.update(|m| {
                    if let Some(d) = m.get_mut(&at) {
                        d.fingerprints.insert(key, now);
                    }
                });
                if let Err(e) = r {
                    tracing::warn!(path = %self.path.display(), error = %e, "couldn't store the project config fingerprint");
                }
                State::Trusted
            }
        }
    }

    /// Record a decision for `dir` (and everything below it); trusting stores its current fingerprint.
    pub fn set(&self, dir: &Path, trusted: bool) -> io::Result<()> {
        self.set_from(dir, trusted, dir)
    }

    /// Record a decision for `at`, taken in `cwd` (itself or a folder below it):
    /// trusting stores `cwd`'s current fingerprint.
    pub fn set_from(&self, at: &Path, trusted: bool, cwd: &Path) -> io::Result<()> {
        let fp = (trusted && cwd.starts_with(at)).then(|| (rel_key(at, cwd), fingerprint(cwd)));
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

/// What differs between two fingerprints, one line per `PROJECT_CONFIG` entry.
fn changes(old: &Fingerprint, now: &Fingerprint) -> Vec<String> {
    PROJECT_CONFIG
        .iter()
        .filter_map(|name| match (old.get(*name), now.get(*name)) {
            (None, Some(_)) => Some(format!("added {name}")),
            (Some(_), None) => Some(format!("removed {name}")),
            (Some(a), Some(b)) if a != b => Some(format!("changed {name}")),
            _ => None,
        })
        .collect()
}

/// True when `dir` has project config that trust would gate.
pub fn has_project_config(dir: &Path) -> bool {
    PROJECT_CONFIG.iter().any(|p| dir.join(p).exists())
}

/// Resolve trust without a UI: `--trust` wins, then a stored decision whose
/// fingerprint still matches; otherwise untrusted.
/// The interactive 4-choice prompt lives in the binary (it needs the TUI).
pub fn resolve(store: &TrustStore, dir: &Path, cli_trust: bool) -> bool {
    cli_trust || store.state(dir) == State::Trusted
}

/// The fingerprint of `dir`'s project config: each `PROJECT_CONFIG` entry that
/// exists, hashed over its sorted relative paths and file contents. Missing
/// entries are left out, so creating one changes the fingerprint.
pub fn fingerprint(dir: &Path) -> Fingerprint {
    let mut budget = MAX_PATHS;
    PROJECT_CONFIG
        .iter()
        .filter(|name| dir.join(name).exists())
        .map(|name| {
            let mut h = Fnv::new();
            hash_path(&mut h, dir, &dir.join(name), &mut budget);
            (name.to_string(), format!("{:016x}", h.0))
        })
        .collect()
}

fn hash_path(h: &mut Fnv, root: &Path, path: &Path, budget: &mut usize) {
    if *budget == 0 {
        h.write(b"\0cut");
        return;
    }
    *budget -= 1;
    h.write(path.strip_prefix(root).unwrap_or(path).to_string_lossy().as_bytes());
    h.write(b"\0");
    if let Ok(target) = std::fs::read_link(path) {
        h.write(b"->");
        h.write(target.to_string_lossy().as_bytes());
        h.write(b"\0");
    }
    let Ok(meta) = std::fs::metadata(path) else { return };
    if meta.is_dir() {
        let mut kids: Vec<PathBuf> = std::fs::read_dir(path)
            .map(|rd| rd.flatten().map(|e| e.path()).collect())
            .unwrap_or_default();
        kids.sort();
        for k in kids {
            hash_path(h, root, &k, budget);
        }
    } else if meta.len() > MAX_FILE {
        let mtime = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or(0, |d| d.as_nanos());
        h.write(&meta.len().to_le_bytes());
        h.write(&mtime.to_le_bytes());
    } else if let Ok(bytes) = std::fs::read(path) {
        h.write(&(bytes.len() as u64).to_le_bytes());
        h.write(&bytes);
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
}
