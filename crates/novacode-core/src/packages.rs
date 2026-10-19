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

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

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

    fn read_lock(&self) -> Lock {
        std::fs::read_to_string(&self.lock)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default()
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

    /// Install `src` (an existing folder, else a git URL) as `name`, at `rev` when
    /// given (a lockfile's pin), else its newest commit, which is then pinned.
    pub fn install(&self, src: &str, name: Option<&str>, rev: Option<&str>) -> Result<String, String> {
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
        if local.is_dir() {
            let abs = std::fs::canonicalize(local).map_err(|e| e.to_string())?;
            std::os::unix::fs::symlink(&abs, &dest).map_err(|e| e.to_string())?;
            return Ok(name);
        }
        git(&["clone", "-q", src, &dest.display().to_string()], None)?;
        if let Some(r) = rev
            && let Err(e) = git(&["checkout", "-q", r], Some(&dest))
        {
            let _ = std::fs::remove_dir_all(&dest);
            return Err(format!("`{name}`: pinned commit {r} not found: {e}"));
        }
        let mut lock = self.read_lock();
        lock.insert(
            name.clone(),
            Pin {
                source: src.into(),
                rev: head(&dest)?,
            },
        );
        self.write_lock(&lock)?;
        Ok(name)
    }

    /// Remove a package: unlink a local one, delete a clone, drop its pin.
    pub fn uninstall(&self, name: &str) -> Result<(), String> {
        let p = self
            .list()
            .into_iter()
            .find(|p| p.name == name)
            .ok_or(format!("`{name}` is not installed"))?;
        if p.link.is_some() {
            std::fs::remove_file(&p.dir).map_err(|e| e.to_string())?;
        } else {
            std::fs::remove_dir_all(&p.dir).map_err(|e| e.to_string())?;
        }
        let mut lock = self.read_lock();
        if lock.remove(name).is_some() {
            self.write_lock(&lock)?;
        }
        Ok(())
    }

    /// Move every clone (or one) to its branch's newest commit and pin that.
    /// Linked folders are always current. Returns (name, new pin or error).
    pub fn update(&self, only: Option<&str>) -> Vec<(String, Result<String, String>)> {
        let mut lock = self.read_lock();
        let out: Vec<_> = self
            .list()
            .into_iter()
            .filter(|p| p.link.is_none() && only.is_none_or(|o| o == p.name))
            .map(|p| {
                // A clone checked out at a pin is on a detached HEAD: go back to the default branch.
                let r = git(&["checkout", "-q", "-"], Some(&p.dir))
                    .or(Ok::<(), String>(()))
                    .and_then(|_| git(&["pull", "--ff-only", "-q"], Some(&p.dir)))
                    .and_then(|_| head(&p.dir));
                if let Ok(rev) = &r {
                    let source = lock.get(&p.name).map(|x| x.source.clone()).unwrap_or_default();
                    lock.insert(
                        p.name.clone(),
                        Pin {
                            source,
                            rev: rev.clone(),
                        },
                    );
                }
                (p.name, r)
            })
            .collect();
        if let Err(e) = self.write_lock(&lock) {
            tracing::warn!("packages lock: {e}");
        }
        out
    }
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
/// lockfile has one. Returns (name, result).
pub fn install_project(cwd: &Path) -> Vec<(String, Result<String, String>)> {
    let store = Store::project(cwd);
    let lock = store.read_lock();
    let want = declared(cwd);
    missing(cwd)
        .into_iter()
        .map(|n| {
            let src = want[&n].clone();
            let rev = lock.get(&n).filter(|p| p.source == src).map(|p| p.rev.clone());
            let r = store.install(&src, Some(&n), rev.as_deref());
            (n, r)
        })
        .collect()
}

/// The package name a source would get: its last path segment without `.git`.
pub fn name_of(src: &str) -> Option<String> {
    let last = src.trim_end_matches('/').rsplit(['/', ':']).next()?;
    let n = last.trim_end_matches(".git");
    (!n.is_empty() && n != "." && n != "..").then(|| n.to_string())
}

fn git_out(args: &[&str], cwd: Option<&Path>) -> Result<String, String> {
    let mut c = std::process::Command::new("git");
    c.args(args).env("GIT_TERMINAL_PROMPT", "0");
    if let Some(d) = cwd {
        c.current_dir(d);
    }
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

    #[test]
    fn install_pin_update_uninstall() {
        let home = tempfile::tempdir().unwrap();
        let store = Store::global(home.path());
        let src = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(src.path().join("skills")).unwrap();
        assert_eq!(name_of("https://github.com/a/my-pack.git").as_deref(), Some("my-pack"));
        assert_eq!(name_of("git@github.com:a/b").as_deref(), Some("b"));
        assert_eq!(
            store
                .install(src.path().to_str().unwrap(), Some("local"), None)
                .unwrap(),
            "local"
        );
        let (r, g) = repo();
        commit(r.path(), &g, "one");
        let url = format!("file://{}", r.path().display());
        store.install(&url, Some("cloned"), None).unwrap();
        let first = store.read_lock()["cloned"].rev.clone();
        assert_eq!(first.len(), 40, "the commit is pinned");
        let l = store.list();
        assert_eq!(
            l.iter().map(|p| p.name.as_str()).collect::<Vec<_>>(),
            ["cloned", "local"]
        );
        assert!(l[1].link.is_some() && l[1].dir.join("skills").is_dir());
        assert!(
            !store.lock.exists() || !store.read_lock().contains_key("local"),
            "links aren't pinned"
        );
        assert!(
            store
                .install(src.path().to_str().unwrap(), Some("local"), None)
                .is_err()
        );

        commit(r.path(), &g, "two");
        let up = store.update(None);
        assert!(up.iter().all(|(_, r)| r.is_ok()), "{:?}", up);
        let second = store.read_lock()["cloned"].rev.clone();
        assert_ne!(first, second, "update moves the pin");

        // A fresh machine with the old lock gets the old commit.
        let other = tempfile::tempdir().unwrap();
        let s2 = Store::global(other.path());
        s2.install(&url, Some("cloned"), Some(&first)).unwrap();
        assert_eq!(head(&s2.dir.join("cloned")).unwrap(), first);
        assert!(!s2.dir.join("cloned/two").exists());

        store.uninstall("local").unwrap();
        store.uninstall("cloned").unwrap();
        assert!(store.list().is_empty() && store.read_lock().is_empty());
        assert!(src.path().join("skills").is_dir(), "the linked source is untouched");
    }

    #[test]
    fn manifest_api_range_and_project_packages() {
        let home = tempfile::tempdir().unwrap();
        let proj = tempfile::tempdir().unwrap();
        let mk = |name: &str, manifest: Option<&str>| {
            let d = tempfile::tempdir().unwrap().keep();
            if let Some(m) = manifest {
                std::fs::write(d.join("novacode-package.json"), m).unwrap();
            }
            Store::global(home.path())
                .install(d.to_str().unwrap(), Some(name), None)
                .unwrap();
        };
        mk("plain", None);
        mk("ok", Some(r#"{"name": "ok", "apiVersion": [1, 3]}"#));
        mk("future", Some(r#"{"apiVersion": 2}"#));
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
        let url = format!("file://{}", r.path().display());
        std::fs::create_dir_all(proj.path().join(".novacode")).unwrap();
        std::fs::write(
            proj.path().join(".novacode/packages.json"),
            format!(r#"{{"packages": {{"team": "{url}"}}}}"#),
        )
        .unwrap();
        assert_eq!(missing(proj.path()), ["team"]);
        assert!(install_project(proj.path()).iter().all(|(_, r)| r.is_ok()));
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
