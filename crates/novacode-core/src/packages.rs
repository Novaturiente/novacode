//! Packages (L7.7): a folder with any of `extensions/`, `skills/`, `prompts/`,
//! installed from git (cloned) or a local path (linked, so edits apply at once)
//! into `~/.novacode/packages/<name>`. npm is not supported.

use std::path::{Path, PathBuf};

pub struct Package {
    pub name: String,
    pub dir: PathBuf,
    /// Linked local folder, or None for a git clone.
    pub link: Option<PathBuf>,
}

fn root(home: &Path) -> PathBuf {
    home.join("packages")
}

/// Installed packages, sorted by name.
pub fn list(home: &Path) -> Vec<Package> {
    let Ok(rd) = std::fs::read_dir(root(home)) else {
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
            dir.is_dir().then_some(Package { name, dir, link })
        })
        .collect();
    v.sort_by(|a, b| a.name.cmp(&b.name));
    v
}

/// The package name a source would get: its last path segment without `.git`.
pub fn name_of(src: &str) -> Option<String> {
    let last = src.trim_end_matches('/').rsplit(['/', ':']).next()?;
    let n = last.trim_end_matches(".git");
    (!n.is_empty() && n != "." && n != "..").then(|| n.to_string())
}

fn git(args: &[&str], cwd: Option<&Path>) -> Result<(), String> {
    let mut c = std::process::Command::new("git");
    c.args(args).env("GIT_TERMINAL_PROMPT", "0");
    if let Some(d) = cwd {
        c.current_dir(d);
    }
    let o = c.output().map_err(|e| format!("git: {e}"))?;
    if o.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&o.stderr).trim().to_string())
    }
}

/// Install `src` (an existing folder, else a git URL). Returns the package name.
pub fn install(home: &Path, src: &str, name: Option<&str>) -> Result<String, String> {
    let name = match name {
        Some(n) => n.to_string(),
        None => name_of(src).ok_or("can't tell the package name; pass --name")?,
    };
    if name.contains('/') || name.starts_with('.') {
        return Err(format!("bad package name `{name}`"));
    }
    let dest = root(home).join(&name);
    if dest.exists() || std::fs::symlink_metadata(&dest).is_ok() {
        return Err(format!(
            "`{name}` is already installed (uninstall it first, or `novacode update {name}`)"
        ));
    }
    std::fs::create_dir_all(root(home)).map_err(|e| e.to_string())?;
    let local = Path::new(src);
    if local.is_dir() {
        let abs = std::fs::canonicalize(local).map_err(|e| e.to_string())?;
        std::os::unix::fs::symlink(&abs, &dest).map_err(|e| e.to_string())?;
    } else {
        git(&["clone", "--depth", "1", src, &dest.display().to_string()], None)?;
    }
    Ok(name)
}

/// Remove a package: unlink a local one, delete a clone.
pub fn uninstall(home: &Path, name: &str) -> Result<(), String> {
    let p = list(home)
        .into_iter()
        .find(|p| p.name == name)
        .ok_or(format!("`{name}` is not installed"))?;
    if p.link.is_some() {
        std::fs::remove_file(&p.dir).map_err(|e| e.to_string())
    } else {
        std::fs::remove_dir_all(&p.dir).map_err(|e| e.to_string())
    }
}

/// `git pull --ff-only` every clone (or one); linked folders are always current.
pub fn update(home: &Path, only: Option<&str>) -> Vec<(String, Result<(), String>)> {
    list(home)
        .into_iter()
        .filter(|p| p.link.is_none() && only.is_none_or(|o| o == p.name))
        .map(|p| {
            let r = git(&["pull", "--ff-only", "-q"], Some(&p.dir));
            (p.name, r)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn install_list_update_uninstall() {
        let home = tempfile::tempdir().unwrap();
        let src = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(src.path().join("skills")).unwrap();
        assert_eq!(name_of("https://github.com/a/my-pack.git").as_deref(), Some("my-pack"));
        assert_eq!(name_of("git@github.com:a/b").as_deref(), Some("b"));
        let n = install(home.path(), src.path().to_str().unwrap(), Some("local")).unwrap();
        assert_eq!(n, "local");
        // A git source: a repo made on the spot.
        let repo = tempfile::tempdir().unwrap();
        let g = |a: &[&str]| git(a, Some(repo.path())).unwrap();
        g(&["init", "-q"]);
        std::fs::write(repo.path().join("README"), "x").unwrap();
        g(&["add", "."]);
        g(&["-c", "user.email=t@t", "-c", "user.name=t", "commit", "-qm", "init"]);
        install(
            home.path(),
            &format!("file://{}", repo.path().display()),
            Some("cloned"),
        )
        .unwrap();
        let l = list(home.path());
        assert_eq!(
            l.iter().map(|p| p.name.as_str()).collect::<Vec<_>>(),
            ["cloned", "local"]
        );
        assert!(l[1].link.is_some() && l[1].dir.join("skills").is_dir());
        assert!(install(home.path(), src.path().to_str().unwrap(), Some("local")).is_err());
        assert!(update(home.path(), None).iter().all(|(_, r)| r.is_ok()));
        uninstall(home.path(), "local").unwrap();
        uninstall(home.path(), "cloned").unwrap();
        assert!(list(home.path()).is_empty());
        assert!(src.path().join("skills").is_dir(), "the linked source is untouched");
    }
}
