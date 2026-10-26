//! Packages (L7.7): a folder with any of `extensions/`, `skills/`, `prompts/`,
//! `agents/`, `themes/` and an `mcp.json`, installed from git (cloned, its commit
//! pinned in a lockfile) or a local path (linked, so edits apply at once). npm is
//! not supported.
//!
//! Two stores, the same layout: `~/.novacode/packages/` with
//! `~/.novacode/packages.lock.json`, and a trusted project's `.novacode/packages/`
//! with `.novacode/packages.lock.json` (committed), whose wanted packages are
//! declared in `.novacode/packages.json` as `{"packages": {name: source}}`.
//!
//! An optional `novacode-package.json` manifest (`{name, version, apiVersion}`,
//! `apiVersion` a number or `[min, max]`) is checked against this novacode's
//! extension API: a package that needs another version is disabled with a reason.
//! A Claude Code plugin installs the same way; [`crate::plugin`] reads its parts.
//!
//! Versions follow releases (L7.15): install and update take the latest release
//! (GitHub's "Latest" when the API answers, else the highest `vX.Y.Z` tag). A
//! repo without releases stays at its commit unless `update --commits` is asked.
//! `--ref` pins a tag (still updated to newer releases), a branch (followed) or a
//! commit.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct Package {
    pub name: String,
    pub dir: PathBuf,
    /// Linked local folder, or None for a git clone.
    pub link: Option<PathBuf>,
    /// Why it doesn't load (its manifest wants another extension API).
    pub disabled: Option<String>,
}

/// One installed git package: where from and the pinned commit.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Pin {
    pub source: String,
    pub rev: String,
    /// The release tag it is at; updates move it to newer releases.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tag: Option<String>,
    /// The branch it follows (`--ref <branch>`); updates take its newest commit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
}

/// Which version to install.
pub enum Want<'a> {
    /// The latest release, else the newest commit.
    Latest,
    /// `--ref`: a tag, branch or commit.
    Ref(&'a str),
    /// A lockfile's pin.
    Pin(&'a Pin),
}

type Lock = BTreeMap<String, Pin>;

/// A package store: the folder packages live in and its lockfile.
pub struct Store {
    pub dir: PathBuf,
    pub lock: PathBuf,
}

impl Store {
    pub fn global(home: &Path) -> Self {
        Store {
            dir: home.join("packages"),
            lock: home.join("packages.lock.json"),
        }
    }

    pub fn project(cwd: &Path) -> Self {
        let d = cwd.join(".novacode");
        Store {
            dir: d.join("packages"),
            lock: d.join("packages.lock.json"),
        }
    }

    /// The lockfile; empty when it doesn't exist. One that can't be read or isn't
    /// valid JSON is an error, so nothing rewrites it and loses its pins.
    fn read_lock(&self) -> Result<Lock, String> {
        let text = match std::fs::read_to_string(&self.lock) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Lock::new()),
            Err(e) => return Err(format!("{}: {e}", self.lock.display())),
        };
        serde_json::from_str(&text).map_err(|e| format!("{}: {e} (fix or delete it)", self.lock.display()))
    }

    fn write_lock(&self, lock: &Lock) -> Result<(), String> {
        let text = serde_json::to_string_pretty(lock).map_err(|e| e.to_string())? + "\n";
        novacode_base::storage::write_atomic(&self.lock, text.as_bytes(), novacode_base::storage::Durability::Durable)
            .map_err(|e| e.to_string())
    }

    /// Installed packages, sorted by name.
    pub fn list(&self) -> Vec<Package> {
        let Ok(rd) = std::fs::read_dir(&self.dir) else {
            return vec![];
        };
        let mut v: Vec<Package> = rd
            .flatten()
            .filter_map(|e| {
                let name = e.file_name().to_str()?.to_string();
                if name.starts_with('.') {
                    return None;
                }
                let dir = e.path();
                let link = std::fs::read_link(&dir).ok();
                let disabled = check_api(&dir).err();
                dir.is_dir().then_some(Package {
                    name,
                    dir,
                    link,
                    disabled,
                })
            })
            .collect();
        v.sort_by(|a, b| a.name.cmp(&b.name));
        v
    }

    /// Install `src` (an existing folder, else a git URL) as `name`, at the version
    /// `want` says, which is then pinned.
    pub async fn install(&self, src: &str, name: Option<&str>, want: Want<'_>) -> Result<String, String> {
        let name = match name {
            Some(n) => n.to_string(),
            None => name_of(src).ok_or("can't tell the package name; pass --name")?,
        };
        if name.contains('/') || name.starts_with('.') {
            return Err(format!("bad package name `{name}`"));
        }
        let dest = self.dir.join(&name);
        if dest.exists() || std::fs::symlink_metadata(&dest).is_ok() {
            return Err(format!(
                "`{name}` is already installed (uninstall it first, or `novacode packages update {name}`)"
            ));
        }
        std::fs::create_dir_all(&self.dir).map_err(|e| e.to_string())?;
        let local = Path::new(src);
        let lock = if local.is_dir() { None } else { Some(self.read_lock()?) };
        if local.is_dir() {
            let abs = novacode_base::platform::canonicalize(local).map_err(|e| e.to_string())?;
            novacode_base::platform::link_dir(&abs, &dest).map_err(|e| e.to_string())?;
            return Ok(name);
        }
        git(&["clone", "-q", src, &dest.display().to_string()], None)?;
        let pin = match checkout_wanted(src, &dest, want).await {
            Ok(p) => p,
            Err(e) => {
                let _ = std::fs::remove_dir_all(&dest);
                return Err(format!("`{name}`: {e}"));
            }
        };
        let mut lock = lock.unwrap_or_default();
        lock.insert(name.clone(), pin);
        self.write_lock(&lock)?;
        Ok(name)
    }

    /// The pin of an installed clone (none when the lockfile is damaged).
    pub fn pin(&self, name: &str) -> Option<Pin> {
        self.read_lock().ok()?.remove(name)
    }

    /// Remove a package: unlink a local one, delete a clone, drop its pin.
    /// `what` is its name, or the URL/path it was installed from. Returns the name.
    pub fn uninstall(&self, what: &str) -> Result<String, String> {
        let lock = self.read_lock()?;
        let norm = |s: &str| s.trim_end_matches('/').trim_end_matches(".git").to_string();
        let local = novacode_base::platform::canonicalize(what).ok();
        let p = self
            .list()
            .into_iter()
            .find(|p| p.name == what)
            .or_else(|| {
                self.list().into_iter().find(|p| {
                    lock.get(&p.name).is_some_and(|pin| norm(&pin.source) == norm(what))
                        || (local.is_some()
                            && p.link
                                .as_ref()
                                .and_then(|l| novacode_base::platform::canonicalize(l).ok())
                                == local)
                })
            })
            .ok_or(format!("`{what}` is not installed (see `novacode packages`)"))?;
        let name = p.name.clone();
        if p.link.is_some() {
            novacode_base::platform::unlink_dir(&p.dir).map_err(|e| e.to_string())?;
        } else {
            std::fs::remove_dir_all(&p.dir).map_err(|e| e.to_string())?;
        }
        let mut lock = lock;
        if lock.remove(&name).is_some() {
            self.write_lock(&lock)?;
        }
        Ok(name)
    }

    /// The pin of an installed clone; without a lockfile entry, its origin and
    /// checked-out commit (as `update` treats it).
    fn pin_or_head(&self, lock: &Lock, p: &Package) -> Pin {
        lock.get(&p.name).cloned().unwrap_or_else(|| Pin {
            source: git_out(&["remote", "get-url", "origin"], Some(&p.dir)).unwrap_or_default(),
            rev: head(&p.dir).unwrap_or_default(),
            tag: None,
            branch: None,
        })
    }

    /// The version (as [`version_of`]) a clone is at now; `None` when it isn't
    /// installed, is linked, or the lockfile is damaged.
    pub fn current(&self, name: &str) -> Option<String> {
        let lock = self.read_lock().ok()?;
        let p = self.list().into_iter().find(|p| p.name == name && p.link.is_none())?;
        Some(version_of(&self.pin_or_head(&lock, &p)))
    }

    /// L11.13: which clones `update` would move, read-only (no clone is fetched
    /// or changed). A followed branch: its remote head against the pin. Otherwise
    /// the latest release (GitHub's "Latest", else the best remote tag); packages
    /// without releases are left out. A package whose remote can't be asked is
    /// skipped (logged). Returns the updates and how many packages answered.
    pub async fn check(&self) -> (Vec<Available>, usize) {
        let lock = match self.read_lock() {
            Ok(l) => l,
            Err(e) => {
                tracing::warn!("update check: {e}");
                return (vec![], 0);
            }
        };
        let (mut out, mut answered) = (Vec::new(), 0);
        for p in self.list() {
            if p.link.is_some() {
                continue;
            }
            let pin = self.pin_or_head(&lock, &p);
            match remote_facts(&p.dir, &pin).await {
                Ok(facts) => {
                    answered += 1;
                    if let Some(to) = available_to(&pin, &facts) {
                        out.push(Available {
                            name: p.name.clone(),
                            from: version_of(&pin),
                            to,
                        });
                    }
                }
                Err(e) => tracing::warn!(package = %p.name, "update check: {e}"),
            }
        }
        (out, answered)
    }

    /// Move every clone (or one) to its latest release, or a followed branch to
    /// its newest commit. One without releases stays where it is unless
    /// `commits`, which moves it to the newest commit of its default branch.
    /// Linked folders are always current. Returns (name, what happened or error);
    /// a damaged lockfile is an error and nothing is updated.
    pub async fn update(
        &self,
        only: Option<&str>,
        commits: bool,
    ) -> Result<Vec<(String, Result<String, String>)>, String> {
        let mut lock = self.read_lock()?;
        let mut out = Vec::new();
        for p in self.list() {
            if p.link.is_some() || only.is_some_and(|o| o != p.name) {
                continue;
            }
            let old = self.pin_or_head(&lock, &p);
            let r = update_one(&p.dir, &old, commits).await;
            let r = r.map(|new| {
                let msg = describe(&old, &new);
                lock.insert(p.name.clone(), new);
                msg
            });
            out.push((p.name, r));
        }
        if let Err(e) = self.write_lock(&lock) {
            tracing::warn!("packages lock: {e}");
        }
        Ok(out)
    }
}

/// A package update the check found (L11.13): from the version it is at to the
/// one `update` would take (as [`version_of`] writes them).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Available {
    pub name: String,
    pub from: String,
    pub to: String,
}

/// What a package's remote says: a followed branch's head commit, or the latest
/// release tag (`None` without releases).
#[derive(Debug, PartialEq)]
enum Remote {
    Branch(String),
    Release(Option<String>),
}

/// Ask the remote, read-only: `ls-remote` and the GitHub API, never a fetch.
async fn remote_facts(dir: &Path, pin: &Pin) -> Result<Remote, String> {
    if let Some(b) = &pin.branch {
        let out = git_out(&["ls-remote", "origin", &format!("refs/heads/{b}")], Some(dir))?;
        let rev = out
            .split_whitespace()
            .next()
            .ok_or(format!("no branch `{b}` on origin"))?;
        return Ok(Remote::Branch(rev.to_string()));
    }
    let gh = match github(&pin.source) {
        Some((owner, repo)) => github_latest(&owner, &repo).await,
        None => Ok(None),
    };
    release_or_tags(dir, gh)
}

/// GitHub's "Latest" when there is one; else (no GitHub release, another host, or
/// the API can't be asked) the highest remote tag, as [`latest`] does after a fetch.
fn release_or_tags(dir: &Path, gh: Result<Option<String>, String>) -> Result<Remote, String> {
    match gh {
        Ok(Some(t)) => return Ok(Remote::Release(Some(t))),
        Ok(None) => {}
        Err(e) => tracing::debug!("update check: GitHub: {e}; using the remote tags"),
    }
    let out = git_out(&["ls-remote", "--tags", "--refs", "origin"], Some(dir))?;
    let tags = out
        .lines()
        .filter_map(|l| l.split('\t').nth(1)?.strip_prefix("refs/tags/"));
    Ok(Remote::Release(best_tag(tags)))
}

/// The version an update would move `pin` to, given what its remote says; `None`
/// when it is current, has no releases, or the release isn't newer.
fn available_to(pin: &Pin, remote: &Remote) -> Option<String> {
    match remote {
        Remote::Branch(rev) => (*rev != pin.rev).then(|| {
            let b = pin.branch.as_deref().unwrap_or_default();
            format!("{b}@{}", &rev[..rev.len().min(12)])
        }),
        Remote::Release(Some(t)) => {
            (pin.tag.as_deref() != Some(t.as_str()) && newer_or_same(pin.tag.as_deref(), t)).then(|| t.clone())
        }
        Remote::Release(None) => None,
    }
}

/// The version a pin is at, for messages: its tag, branch or short commit.
pub fn version_of(p: &Pin) -> String {
    let short = &p.rev[..p.rev.len().min(12)];
    match (&p.tag, &p.branch) {
        (Some(t), _) => t.clone(),
        (_, Some(b)) => format!("{b}@{short}"),
        _ => short.to_string(),
    }
}

fn describe(old: &Pin, new: &Pin) -> String {
    if old.rev == new.rev {
        return match (&new.tag, &new.branch) {
            (None, None) => format!(
                "no releases; stays at {} (`--commits` moves it to the newest commit)",
                version_of(new)
            ),
            _ => format!("up to date ({})", version_of(new)),
        };
    }
    format!("{} → {}", version_of(old), version_of(new))
}

async fn update_one(dir: &Path, old: &Pin, commits: bool) -> Result<Pin, String> {
    let d = Some(dir);
    git(&["fetch", "-q", "--tags", "--force", "origin"], d)?;
    let at = |rev: String, tag: Option<String>, branch: Option<String>| Pin {
        source: old.source.clone(),
        rev,
        tag,
        branch,
    };
    if let Some(b) = &old.branch {
        git(&["checkout", "-q", "--detach", &format!("origin/{b}")], d)?;
        return Ok(at(head(dir)?, None, Some(b.clone())));
    }
    if let Some(t) = latest(&old.source, dir).await {
        if !newer_or_same(old.tag.as_deref(), &t) {
            return Ok(old.clone());
        }
        git(&["checkout", "-q", "--detach", &format!("refs/tags/{t}")], d)?;
        return Ok(at(head(dir)?, Some(t), None));
    }
    if commits {
        git(&["checkout", "-q", "--detach", "origin/HEAD"], d)?;
        return Ok(at(head(dir)?, None, None));
    }
    Ok(old.clone())
}

/// Check out what `want` asks for in a fresh clone; the pin to record.
async fn checkout_wanted(src: &str, dir: &Path, want: Want<'_>) -> Result<Pin, String> {
    let d = Some(dir);
    let pin = |tag: Option<String>, branch: Option<String>| -> Result<Pin, String> {
        Ok(Pin {
            source: src.into(),
            rev: head(dir)?,
            tag,
            branch,
        })
    };
    match want {
        Want::Pin(p) => {
            git(&["checkout", "-q", "--detach", &p.rev], d)
                .map_err(|e| format!("pinned commit {} not found: {e}", p.rev))?;
            pin(p.tag.clone(), p.branch.clone())
        }
        Want::Ref(r) => {
            if git(
                &["rev-parse", "-q", "--verify", &format!("refs/tags/{r}^{{commit}}")],
                d,
            )
            .is_ok()
            {
                git(&["checkout", "-q", "--detach", &format!("refs/tags/{r}")], d)?;
                pin(Some(r.into()), None)
            } else if git(&["rev-parse", "-q", "--verify", &format!("refs/remotes/origin/{r}")], d).is_ok() {
                git(&["checkout", "-q", "--detach", &format!("origin/{r}")], d)?;
                pin(None, Some(r.into()))
            } else {
                git(&["checkout", "-q", "--detach", r], d)
                    .map_err(|e| format!("no tag, branch or commit `{r}`: {e}"))?;
                pin(None, None)
            }
        }
        Want::Latest => match latest(src, dir).await {
            Some(t) => {
                git(&["checkout", "-q", "--detach", &format!("refs/tags/{t}")], d)?;
                pin(Some(t), None)
            }
            None => pin(None, None),
        },
    }
}

/// The latest release of a clone: GitHub's "Latest" release when the API answers
/// (no token needed), else the highest `vX.Y.Z` tag; `None` without releases.
pub async fn latest(src: &str, dir: &Path) -> Option<String> {
    let have = |t: &str| {
        git(
            &["rev-parse", "-q", "--verify", &format!("refs/tags/{t}^{{commit}}")],
            Some(dir),
        )
        .is_ok()
    };
    if let Some((owner, repo)) = github(src)
        && let Ok(Some(t)) = github_latest(&owner, &repo).await
        && have(&t)
    {
        return Some(t);
    }
    let tags = git_out(&["tag", "-l"], Some(dir)).ok()?;
    best_tag(tags.lines())
}

/// GitHub's "Latest" release tag: `Ok(None)` when the repo has none (404), an
/// error when the API can't be asked (network, rate limit).
async fn github_latest(owner: &str, repo: &str) -> Result<Option<String>, String> {
    let r = reqwest::Client::builder()
        .user_agent(concat!("novacode/", env!("CARGO_PKG_VERSION")))
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .map_err(|e| e.to_string())?
        .get(format!("https://api.github.com/repos/{owner}/{repo}/releases/latest"))
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if r.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    if !r.status().is_success() {
        return Err(format!("GitHub API: {}", r.status()));
    }
    let v: Value = serde_json::from_str(&r.text().await.map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
    Ok(v["tag_name"].as_str().map(str::to_string))
}

/// `owner/repo` of a GitHub URL (https, ssh or scp form).
fn github(src: &str) -> Option<(String, String)> {
    let rest = [
        "https://github.com/",
        "http://github.com/",
        "git@github.com:",
        "ssh://git@github.com/",
    ]
    .iter()
    .find_map(|p| src.strip_prefix(p))?;
    let mut it = rest.trim_end_matches('/').trim_end_matches(".git").split('/');
    let (o, r) = (it.next()?, it.next()?);
    (!o.is_empty() && !r.is_empty() && it.next().is_none()).then(|| (o.to_string(), r.to_string()))
}

/// The version of an `X.Y.Z` tag (`v` and a `name-` prefix allowed); `None` for a
/// tag that isn't a version. The last field is `false` for a pre-release
/// (`v2.0.0-rc1`), which orders just below its release.
fn tag_version(tag: &str) -> Option<(u64, u64, u64, bool)> {
    static RE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"^(?:.*[-_/])?v?(\d+)\.(\d+)\.(\d+)(-[0-9A-Za-z.-]+)?$").unwrap()
    });
    let c = RE.captures(tag.trim())?;
    let n = |i: usize| c[i].parse::<u64>().ok();
    Some((n(1)?, n(2)?, n(3)?, c.get(4).is_none()))
}

/// Whether an update from tag `cur` may move to release `new`: never to an older
/// version. Same tag, or a newer version: yes. When `cur` has no version (no tag,
/// or a tag that isn't one) it can't be compared and the release is taken; a
/// `new` without a version can't be compared to a versioned `cur`, which stays.
fn newer_or_same(cur: Option<&str>, new: &str) -> bool {
    let Some(cur) = cur else { return true };
    if cur == new {
        return true;
    }
    match (tag_version(cur), tag_version(new)) {
        (None, _) => true,
        (Some(_), None) => false,
        (Some(c), Some(n)) => n > c,
    }
}

/// The highest stable `X.Y.Z` tag (`v` and a `name-` prefix allowed; pre-releases
/// skipped). On a tie the shorter name wins (`v1.2.0` over `bin-v1.2.0`).
fn best_tag<'a>(tags: impl Iterator<Item = &'a str>) -> Option<String> {
    tags.filter_map(|t| {
        tag_version(t)
            .filter(|v| v.3)
            .map(|v| (v, std::cmp::Reverse(t.len()), t.trim().to_string()))
    })
    .max()
    .map(|(_, _, t)| t)
}

/// Packages that load in this session, in precedence order: a trusted project's
/// first, then the user's. Disabled ones are left out (see [`problems`]).
pub fn enabled(home: &Path, cwd: &Path, trusted: bool) -> Vec<Package> {
    let mut v = Vec::new();
    if trusted {
        v.extend(Store::project(cwd).list());
    }
    v.extend(Store::global(home).list());
    v.retain(|p| p.disabled.is_none());
    v
}

/// One line per package that doesn't load, and why.
pub fn problems(home: &Path, cwd: &Path, trusted: bool) -> Vec<String> {
    let mut v = Store::global(home).list();
    if trusted {
        v.extend(Store::project(cwd).list());
    }
    v.into_iter()
        .filter_map(|p| p.disabled.map(|why| format!("package `{}` disabled: {why}", p.name)))
        .collect()
}

/// The manifest's `apiVersion` against this novacode's extension API.
fn check_api(dir: &Path) -> Result<(), String> {
    let Ok(text) = std::fs::read_to_string(dir.join("novacode-package.json")) else {
        return Ok(()); // no manifest: nothing to check
    };
    let m: Value = serde_json::from_str(&text).map_err(|e| format!("novacode-package.json: {e}"))?;
    let have = novacode_script::ext::API_VERSION as u64;
    let (lo, hi) = match &m["apiVersion"] {
        Value::Null => return Ok(()),
        Value::Number(n) => (n.as_u64(), n.as_u64()),
        Value::Array(a) if a.len() == 2 => (a[0].as_u64(), a[1].as_u64()),
        v => {
            return Err(format!(
                "novacode-package.json: apiVersion `{v}` is not a number or [min, max]"
            ));
        }
    };
    match (lo, hi) {
        (Some(lo), Some(hi)) if (lo..=hi).contains(&have) => Ok(()),
        (Some(lo), Some(hi)) => Err(format!(
            "it needs extension API {}, this novacode has {have}",
            if lo == hi {
                lo.to_string()
            } else {
                format!("{lo}–{hi}")
            }
        )),
        _ => Err("novacode-package.json: apiVersion must be whole numbers".into()),
    }
}

/// `.novacode/packages.json` of a project: the packages it wants, by name.
pub fn declared(cwd: &Path) -> BTreeMap<String, String> {
    #[derive(Deserialize)]
    struct F {
        #[serde(default)]
        packages: BTreeMap<String, String>,
    }
    std::fs::read_to_string(cwd.join(".novacode").join("packages.json"))
        .ok()
        .and_then(|t| serde_json::from_str::<F>(&t).ok())
        .map(|f| f.packages)
        .unwrap_or_default()
}

/// Declared project packages that aren't installed yet.
pub fn missing(cwd: &Path) -> Vec<String> {
    let have: Vec<String> = Store::project(cwd).list().into_iter().map(|p| p.name).collect();
    declared(cwd).into_keys().filter(|n| !have.contains(n)).collect()
}

/// Install the project's missing packages, each at its pinned commit when the
/// lockfile has one, else its latest release. Returns (name, result).
pub async fn install_project(cwd: &Path) -> Vec<(String, Result<String, String>)> {
    let store = Store::project(cwd);
    let lock = match store.read_lock() {
        Ok(l) => l,
        Err(e) => return missing(cwd).into_iter().map(|n| (n, Err(e.clone()))).collect(),
    };
    let want = declared(cwd);
    let mut out = Vec::new();
    for n in missing(cwd) {
        let src = want[&n].clone();
        let pin = lock.get(&n).filter(|p| p.source == src);
        let w = pin.map_or(Want::Latest, Want::Pin);
        let r = store.install(&src, Some(&n), w).await;
        out.push((n, r));
    }
    out
}

/// The package name a source would get: its last path segment without `.git`.
pub fn name_of(src: &str) -> Option<String> {
    let last = src.trim_end_matches('/').rsplit(['/', ':']).next()?;
    let n = last.trim_end_matches(".git");
    (!n.is_empty() && n != "." && n != "..").then(|| n.to_string())
}

fn git_out(args: &[&str], cwd: Option<&Path>) -> Result<String, String> {
    let mut c = match cwd {
        Some(d) => novacode_base::git::command_in(d),
        None => novacode_base::git::command(),
    };
    c.args(args).env("GIT_TERMINAL_PROMPT", "0");
    let o = c.output().map_err(|e| format!("git: {e}"))?;
    if o.status.success() {
        Ok(String::from_utf8_lossy(&o.stdout).trim().to_string())
    } else {
        Err(String::from_utf8_lossy(&o.stderr).trim().to_string())
    }
}

fn git(args: &[&str], cwd: Option<&Path>) -> Result<(), String> {
    git_out(args, cwd).map(|_| ())
}

fn head(dir: &Path) -> Result<String, String> {
    git_out(&["rev-parse", "HEAD"], Some(dir))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `file:///C:/x` on Windows, `file:///x` elsewhere.
    fn file_url(p: &Path) -> String {
        let s = novacode_base::platform::slash_path(p);
        if s.starts_with('/') {
            format!("file://{s}")
        } else {
            format!("file:///{s}")
        }
    }

    fn repo() -> (tempfile::TempDir, impl Fn(&[&str])) {
        let repo = tempfile::tempdir().unwrap();
        let p = repo.path().to_path_buf();
        let g = move |a: &[&str]| git(a, Some(&p)).unwrap();
        g(&["init", "-q", "-b", "main"]);
        (repo, g)
    }

    fn commit(dir: &Path, g: &impl Fn(&[&str]), file: &str) {
        std::fs::write(dir.join(file), "x").unwrap();
        g(&["add", "."]);
        g(&["-c", "user.email=t@t", "-c", "user.name=t", "commit", "-qm", file]);
    }

    #[tokio::test]
    async fn install_pin_update_uninstall() {
        let home = tempfile::tempdir().unwrap();
        let store = Store::global(home.path());
        let src = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(src.path().join("skills")).unwrap();
        assert_eq!(name_of("https://github.com/a/my-pack.git").as_deref(), Some("my-pack"));
        assert_eq!(name_of("git@github.com:a/b").as_deref(), Some("b"));
        assert_eq!(
            store
                .install(src.path().to_str().unwrap(), Some("local"), Want::Latest)
                .await
                .unwrap(),
            "local"
        );
        let (r, g) = repo();
        commit(r.path(), &g, "one");
        let url = file_url(r.path());
        store.install(&url, Some("cloned"), Want::Latest).await.unwrap();
        let first = store.read_lock().unwrap()["cloned"].rev.clone();
        assert_eq!(first.len(), 40, "the commit is pinned");
        let l = store.list();
        assert_eq!(
            l.iter().map(|p| p.name.as_str()).collect::<Vec<_>>(),
            ["cloned", "local"]
        );
        assert!(l[1].link.is_some() && l[1].dir.join("skills").is_dir());
        assert!(
            !store.lock.exists() || !store.read_lock().unwrap().contains_key("local"),
            "links aren't pinned"
        );
        assert!(
            store
                .install(src.path().to_str().unwrap(), Some("local"), Want::Latest)
                .await
                .is_err()
        );

        // No releases: a plain update leaves it; --commits moves it.
        commit(r.path(), &g, "two");
        let up = store.update(None, false).await.unwrap();
        assert!(up[0].1.as_ref().unwrap().contains("no releases"), "{up:?}");
        assert_eq!(store.read_lock().unwrap()["cloned"].rev, first, "untagged stays pinned");
        let up = store.update(None, true).await.unwrap();
        assert!(up.iter().all(|(_, r)| r.is_ok()), "{:?}", up);
        let second = store.read_lock().unwrap()["cloned"].rev.clone();
        assert_ne!(first, second, "--commits moves the pin");

        // Releases appear: a plain update takes the highest one.
        let tag = |t: &str| g(&["tag", t]);
        tag("v1.0.0");
        commit(r.path(), &g, "three");
        tag("v1.2.0");
        tag("bin-v1.2.0");
        commit(r.path(), &g, "four");
        tag("v2.0.0-rc1");
        let up = store.update(Some("cloned"), false).await.unwrap();
        assert!(up[0].1.as_ref().unwrap().ends_with("→ v1.2.0"), "{up:?}");
        let pin = store.pin("cloned").unwrap();
        assert_eq!(pin.tag.as_deref(), Some("v1.2.0"));
        assert!(store.dir.join("cloned/three").exists() && !store.dir.join("cloned/four").exists());
        assert!(
            store.update(None, false).await.unwrap()[0]
                .1
                .as_ref()
                .unwrap()
                .starts_with("up to date")
        );

        // A fresh machine with the old lock gets the old commit.
        let other = tempfile::tempdir().unwrap();
        let s2 = Store::global(other.path());
        let old = Pin {
            source: url.clone(),
            rev: first.clone(),
            tag: None,
            branch: None,
        };
        s2.install(&url, Some("cloned"), Want::Pin(&old)).await.unwrap();
        assert_eq!(head(&s2.dir.join("cloned")).unwrap(), first);
        assert!(!s2.dir.join("cloned/two").exists());
        // A fresh install takes the latest release; --ref a tag, a branch.
        s2.install(&url, Some("latest"), Want::Latest).await.unwrap();
        assert_eq!(s2.pin("latest").unwrap().tag.as_deref(), Some("v1.2.0"));
        s2.install(&url, Some("old"), Want::Ref("v1.0.0")).await.unwrap();
        assert!(!s2.dir.join("old/three").exists());
        let up = s2.update(Some("old"), false).await.unwrap();
        assert!(
            up[0].1.as_ref().unwrap().contains("v1.0.0 → v1.2.0"),
            "a pinned tag still moves: {up:?}"
        );
        // The latest release is older than the installed tag (a release was
        // withdrawn): the package stays, never moves back.
        tag("v3.0.0");
        s2.install(&url, Some("ahead"), Want::Ref("v3.0.0")).await.unwrap();
        g(&["tag", "-d", "v3.0.0"]);
        git(&["tag", "-d", "v3.0.0"], Some(&s2.dir.join("ahead"))).unwrap();
        let before = head(&s2.dir.join("ahead")).unwrap();
        let up = s2.update(Some("ahead"), false).await.unwrap();
        assert_eq!(up[0].1.as_deref(), Ok("up to date (v3.0.0)"), "{up:?}");
        assert_eq!(s2.pin("ahead").unwrap().tag.as_deref(), Some("v3.0.0"));
        assert_eq!(head(&s2.dir.join("ahead")).unwrap(), before);
        s2.install(&url, Some("edge"), Want::Ref("main")).await.unwrap();
        assert_eq!(s2.pin("edge").unwrap().branch.as_deref(), Some("main"));
        commit(r.path(), &g, "five");
        s2.update(Some("edge"), false).await.unwrap();
        assert!(s2.dir.join("edge/five").exists(), "a branch is followed");
        assert!(s2.install(&url, Some("bad"), Want::Ref("nope")).await.is_err());
        assert!(!s2.dir.join("bad").exists(), "a failed install leaves nothing");

        // By the path or URL it was installed from, not only by name.
        assert_eq!(store.uninstall(src.path().to_str().unwrap()).unwrap(), "local");
        assert_eq!(store.uninstall(&format!("{url}/")).unwrap(), "cloned");
        assert!(store.uninstall("cloned").is_err());
        assert!(store.list().is_empty() && store.read_lock().unwrap().is_empty());
        assert!(src.path().join("skills").is_dir(), "the linked source is untouched");
    }

    #[tokio::test]
    async fn damaged_lock_is_an_error_and_left_alone() {
        let home = tempfile::tempdir().unwrap();
        let store = Store::global(home.path());
        assert_eq!(store.read_lock(), Ok(Lock::new()), "a missing lock is empty");
        let (r, g) = repo();
        commit(r.path(), &g, "one");
        let url = file_url(r.path());
        store.install(&url, Some("a"), Want::Latest).await.unwrap();
        std::fs::write(&store.lock, "{broken").unwrap();
        let named = |e: String| {
            assert!(
                e.contains("packages.lock.json") && e.contains("key must be a string"),
                "{e}"
            );
        };
        named(store.install(&url, Some("b"), Want::Latest).await.unwrap_err());
        assert!(!store.dir.join("b").exists(), "nothing cloned");
        named(store.update(None, false).await.unwrap_err());
        named(store.uninstall("a").unwrap_err());
        assert!(store.dir.join("a").is_dir(), "nothing removed");
        assert_eq!(store.pin("a"), None);
        assert_eq!(
            std::fs::read_to_string(&store.lock).unwrap(),
            "{broken",
            "never rewritten"
        );

        // A project's damaged lock: each missing package fails, nothing installs.
        let proj = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(proj.path().join(".novacode")).unwrap();
        std::fs::write(
            proj.path().join(".novacode/packages.json"),
            format!(r#"{{"packages": {{"team": "{url}"}}}}"#),
        )
        .unwrap();
        std::fs::write(proj.path().join(".novacode/packages.lock.json"), "[").unwrap();
        let r = install_project(proj.path()).await;
        assert!(
            r.len() == 1 && r[0].1.as_ref().is_err_and(|e| e.contains("packages.lock.json")),
            "{r:?}"
        );
        assert_eq!(missing(proj.path()), ["team"]);
        assert_eq!(
            std::fs::read_to_string(proj.path().join(".novacode/packages.lock.json")).unwrap(),
            "["
        );
    }

    #[test]
    fn never_back_to_an_older_release() {
        for (cur, new, move_) in [
            (None, "v1.0.0", true),
            (Some("v1.0.0"), "v1.2.0", true),
            (Some("1.0.0"), "v1.0.1", true),
            (Some("v1.10.0"), "v1.9.0", false),
            (Some("v2.0.0"), "v1.2.0", false),
            (Some("v1.2.0"), "v1.2.0", true),
            (Some("v1.2.0"), "bin-v1.2.0", false),
            (Some("nightly"), "v1.0.0", true),
            (Some("v2.0.0-rc1"), "v1.0.0", false),
            (Some("v2.0.0-rc1"), "v1.9.9", false),
            (Some("v2.0.0-rc1"), "v2.0.0", true),
            (Some("bin-v2.0.0-rc1"), "v2.0.1", true),
            (Some("v2.0.0"), "v2.0.0-rc1", false),
            (Some("v1.0.0"), "nightly", false),
            (Some("edge"), "nightly", true),
        ] {
            assert_eq!(newer_or_same(cur, new), move_, "{cur:?} -> {new}");
        }
    }

    #[test]
    fn check_decides_from_remote_facts() {
        let pin = |tag: Option<&str>, branch: Option<&str>| Pin {
            source: "https://example.com/a/b".into(),
            rev: "a".repeat(40),
            tag: tag.map(Into::into),
            branch: branch.map(Into::into),
        };
        let rel = |t: Option<&str>| Remote::Release(t.map(Into::into));
        let at = pin(Some("v4.0.1"), None);
        assert_eq!(available_to(&at, &rel(Some("v4.1.0"))).as_deref(), Some("v4.1.0"));
        assert_eq!(available_to(&at, &rel(Some("v4.0.1"))), None, "current");
        assert_eq!(available_to(&at, &rel(Some("v3.9.0"))), None, "older release");
        assert_eq!(available_to(&at, &rel(None)), None, "no releases");
        let untagged = pin(None, None);
        assert_eq!(available_to(&untagged, &rel(None)), None);
        assert_eq!(available_to(&untagged, &rel(Some("v1.0.0"))).as_deref(), Some("v1.0.0"));
        let edge = pin(None, Some("main"));
        assert_eq!(available_to(&edge, &Remote::Branch("a".repeat(40))), None);
        assert_eq!(
            available_to(&edge, &Remote::Branch("b".repeat(40))).as_deref(),
            Some("main@bbbbbbbbbbbb")
        );
    }

    #[tokio::test]
    async fn check_reads_remote_without_fetching() {
        let home = tempfile::tempdir().unwrap();
        let store = Store::global(home.path());
        let (r, g) = repo();
        commit(r.path(), &g, "one");
        g(&["tag", "v1.0.0"]);
        let url = file_url(r.path());
        store.install(&url, Some("p"), Want::Latest).await.unwrap();
        store.install(&url, Some("edge"), Want::Ref("main")).await.unwrap();
        assert_eq!(store.check().await, (vec![], 2));
        commit(r.path(), &g, "two");
        g(&["tag", "v1.1.0"]);
        let before = head(&store.dir.join("p")).unwrap();
        let (found, answered) = store.check().await;
        assert_eq!(answered, 2);
        assert_eq!(
            found[1],
            Available {
                name: "p".into(),
                from: "v1.0.0".into(),
                to: "v1.1.0".into()
            }
        );
        assert!(found[0].name == "edge" && found[0].to.starts_with("main@"), "{found:?}");
        assert_eq!(head(&store.dir.join("p")).unwrap(), before, "not checked out");
        assert!(
            git(
                &["rev-parse", "-q", "--verify", "refs/tags/v1.1.0"],
                Some(&store.dir.join("p"))
            )
            .is_err(),
            "not fetched"
        );
        assert_eq!(store.current("p").as_deref(), Some("v1.0.0"));
        assert_eq!(store.current("nope"), None);
        // A GitHub repo without a release, or an API error: the remote tags decide.
        let p = store.dir.join("p");
        let tag = |t: &str| Ok(Remote::Release(Some(t.into())));
        assert_eq!(release_or_tags(&p, Ok(None)), tag("v1.1.0"));
        assert_eq!(release_or_tags(&p, Err("rate limited".into())), tag("v1.1.0"));
        assert_eq!(release_or_tags(&p, Ok(Some("v1.0.5".into()))), tag("v1.0.5"));
    }

    #[test]
    fn tags_and_github_urls() {
        assert_eq!(
            best_tag(["v1.9.0", "v1.10.0", "x", "2.0.0-beta"].into_iter()).as_deref(),
            Some("v1.10.0")
        );
        assert_eq!(
            best_tag(["bin-v4.0.1", "v4.0.1"].into_iter()).as_deref(),
            Some("v4.0.1")
        );
        assert_eq!(best_tag(["main"].into_iter()), None);
        let gh = |s| github(s).map(|(o, r)| format!("{o}/{r}"));
        assert_eq!(gh("https://github.com/a/b.git").as_deref(), Some("a/b"));
        assert_eq!(gh("git@github.com:a/b").as_deref(), Some("a/b"));
        assert_eq!(gh("https://gitlab.com/a/b"), None);
    }

    #[tokio::test]
    async fn manifest_api_range_and_project_packages() {
        let home = tempfile::tempdir().unwrap();
        let proj = tempfile::tempdir().unwrap();
        let mk = |_name: &str, manifest: Option<&str>| {
            let d = tempfile::tempdir().unwrap().keep();
            if let Some(m) = manifest {
                std::fs::write(d.join("novacode-package.json"), m).unwrap();
            }
            d
        };
        let install = |name: &'static str, d: PathBuf| {
            let store = Store::global(home.path());
            async move {
                store
                    .install(d.to_str().unwrap(), Some(name), Want::Latest)
                    .await
                    .unwrap()
            }
        };
        install("plain", mk("plain", None)).await;
        install("ok", mk("ok", Some(r#"{"name": "ok", "apiVersion": [1, 3]}"#))).await;
        install("future", mk("future", Some(r#"{"apiVersion": 2}"#))).await;
        let names: Vec<String> = enabled(home.path(), proj.path(), true)
            .into_iter()
            .map(|p| p.name)
            .collect();
        assert_eq!(names, ["ok", "plain"]);
        let why = problems(home.path(), proj.path(), true);
        assert_eq!(why.len(), 1);
        assert!(why[0].contains("needs extension API 2, this novacode has 1"), "{why:?}");

        // Declared in the project, pinned by its committed lock on install.
        let (r, g) = repo();
        commit(r.path(), &g, "one");
        let url = file_url(r.path());
        std::fs::create_dir_all(proj.path().join(".novacode")).unwrap();
        std::fs::write(
            proj.path().join(".novacode/packages.json"),
            format!(r#"{{"packages": {{"team": "{url}"}}}}"#),
        )
        .unwrap();
        assert_eq!(missing(proj.path()), ["team"]);
        assert!(install_project(proj.path()).await.iter().all(|(_, r)| r.is_ok()));
        assert!(missing(proj.path()).is_empty());
        assert!(proj.path().join(".novacode/packages.lock.json").is_file());
        let names: Vec<String> = enabled(home.path(), proj.path(), true)
            .into_iter()
            .map(|p| p.name)
            .collect();
        assert_eq!(names, ["team", "ok", "plain"], "project packages come first");
        assert!(
            !enabled(home.path(), proj.path(), false)
                .iter()
                .any(|p| p.name == "team"),
            "untrusted: not loaded"
        );
    }
}
