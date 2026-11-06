//! `novacode --super` entry (L10.9 "Entry and scope"): one super per root folder.
//!
//! `<home>/super/<root-hash>/` holds `root` (the canonical root path, which catches hash
//! collisions and lets nested roots be found), `super.lock` (held by the running super) and
//! `session` (its session id, written once its socket is up).

use anyhow::bail;
use novacode_base::storage::{FileLock, try_lock};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// `<home>/super/<root-hash>`.
pub fn dir(home: &Path, root: &Path) -> PathBuf {
    home.join("super").join(novacode_core::trust::path_key(root))
}

fn lock_path(dir: &Path) -> PathBuf {
    // `try_lock` adds `.lock`: the file is `<dir>/super.lock`.
    dir.join("super")
}

/// Whether a super (maybe one still shutting down) holds `root`'s lock now.
pub fn locked(home: &Path, root: &Path) -> bool {
    held(&dir(home, root))
}

/// Whether a super holds this folder's lock now.
fn held(dir: &Path) -> bool {
    matches!(try_lock(&lock_path(dir)), Ok(None))
}

/// `root` is written as the path's bytes, so a path that isn't UTF-8 reads back too.
fn read_root(dir: &Path) -> Option<PathBuf> {
    let bytes = std::fs::read(dir.join("root")).ok()?;
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;
        Some(PathBuf::from(std::ffi::OsString::from_vec(bytes)))
    }
    #[cfg(not(unix))]
    {
        String::from_utf8(bytes).ok().map(PathBuf::from)
    }
}

/// Refuses a root inside or containing the root of a live super, and a hash collision.
pub fn check_root(home: &Path, root: &Path) -> anyhow::Result<()> {
    check_collision(home, root)?;
    check_nested(home, root)
}

fn check_collision(home: &Path, root: &Path) -> anyhow::Result<()> {
    let own = dir(home, root);
    if let Some(r) = read_root(&own)
        && r != root
    {
        bail!(
            "{} shares its super folder with {} (hash collision); remove {} to continue",
            root.display(),
            r.display(),
            own.display()
        );
    }
    Ok(())
}

fn check_nested(home: &Path, root: &Path) -> anyhow::Result<()> {
    for e in std::fs::read_dir(home.join("super")).into_iter().flatten().flatten() {
        let d = e.path();
        let Some(other) = read_root(&d) else { continue };
        if other == root || !(root.starts_with(&other) || other.starts_with(root)) || !held(&d) {
            continue;
        }
        let how = if root.starts_with(&other) { "inside" } else { "above" };
        bail!(
            "a super agent runs for {}; one {how} it would mix up task labels",
            other.display()
        );
    }
    Ok(())
}

/// The detached super takes the root's lock. `None`: another super holds it.
/// `root` is written before the root is checked again (a nested launch may have raced
/// it): of two nested claims at once, the later check sees the other. A `session` left
/// by an earlier super is removed.
pub fn claim(home: &Path, root: &Path) -> anyhow::Result<Option<FileLock>> {
    let d = dir(home, root);
    let Some(lock) = try_lock(&lock_path(&d))? else {
        return Ok(None);
    };
    check_collision(home, root)?;
    std::fs::write(d.join("root"), root.as_os_str().as_encoded_bytes())?;
    check_nested(home, root)?;
    match std::fs::remove_file(d.join("session")) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e.into()),
        _ => {}
    }
    Ok(Some(lock))
}

/// Super writes its session id once its socket is up.
pub fn write_session(home: &Path, root: &Path, id: &str) -> std::io::Result<()> {
    let d = dir(home, root);
    novacode_base::storage::write_atomic_no_backup(
        &d.join("session"),
        id.as_bytes(),
        novacode_base::storage::Durability::Fast,
    )
}

/// The running super's session id, when `live` says its socket answers.
pub fn running(home: &Path, root: &Path, live: impl Fn(&str) -> bool) -> Option<String> {
    let id = std::fs::read_to_string(dir(home, root).join("session")).ok()?;
    let id = id.trim();
    (!id.is_empty() && live(id)).then(|| id.to_string())
}

/// The launcher: waits up to `timeout` for a live super's session.
pub fn wait(home: &Path, root: &Path, timeout: Duration, live: impl Fn(&str) -> bool) -> anyhow::Result<String> {
    let end = Instant::now() + timeout;
    loop {
        if let Some(id) = running(home, root, &live) {
            return Ok(id);
        }
        if Instant::now() >= end {
            bail!("the super agent didn't start; see ~/.novacode/logs");
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> (tempfile::TempDir, PathBuf) {
        let t = tempfile::tempdir().unwrap();
        let home = t.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        (t, home)
    }

    #[test]
    fn nested_and_containing_roots_of_a_live_super_are_refused() {
        let (t, home) = setup();
        let root = t.path().join("projects");
        let _held = claim(&home, &root).unwrap().expect("first claim wins");
        let inside = check_root(&home, &root.join("app")).unwrap_err().to_string();
        assert!(inside.contains("inside"), "{inside}");
        let above = check_root(&home, t.path()).unwrap_err().to_string();
        assert!(above.contains("above"), "{above}");
        // A sibling folder shares no prefix.
        check_root(&home, &t.path().join("projects-2")).unwrap();
        // The same root is not "nested": the launcher attaches to it.
        check_root(&home, &root).unwrap();
    }

    #[test]
    fn a_dead_supers_root_does_not_block_nesting() {
        let (t, home) = setup();
        let root = t.path().join("projects");
        // Its folder is left behind, its lock is free. (Not a claim then drop: a test
        // forking at that moment would hold the lock fd a little longer.)
        let d = dir(&home, &root);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("root"), root.to_str().unwrap()).unwrap();
        check_root(&home, &root.join("app")).unwrap();
    }

    #[test]
    fn second_claim_loses_while_the_first_holds() {
        let (t, home) = setup();
        let root = t.path().join("projects");
        let first = claim(&home, &root).unwrap();
        assert!(first.is_some());
        assert!(claim(&home, &root).unwrap().is_none());
    }

    #[test]
    fn claim_writes_root_and_clears_an_old_session() {
        let (t, home) = setup();
        let root = t.path().join("projects");
        let d = dir(&home, &root);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("session"), "old-id").unwrap();
        let _l = claim(&home, &root).unwrap().unwrap();
        assert_eq!(std::fs::read_to_string(d.join("root")).unwrap(), root.to_str().unwrap());
        assert!(running(&home, &root, |_| true).is_none());
        write_session(&home, &root, "new-id").unwrap();
        assert_eq!(running(&home, &root, |id| id == "new-id").as_deref(), Some("new-id"));
        assert!(
            running(&home, &root, |_| false).is_none(),
            "a dead session is not running"
        );
    }

    #[test]
    fn a_hash_collision_is_refused() {
        let (t, home) = setup();
        let root = t.path().join("projects");
        let d = dir(&home, &root);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("root"), "/somewhere/else").unwrap();
        let e = check_root(&home, &root).unwrap_err().to_string();
        assert!(e.contains("hash collision"), "{e}");
        assert!(claim(&home, &root).is_err());
    }

    /// C7: a claimant publishes its root before it looks for nested ones, so of two supers
    /// claiming nested roots at once the later check always sees the other.
    #[test]
    fn a_claim_publishes_its_root_before_checking_for_nested_ones() {
        let (t, home) = setup();
        let root = t.path().join("projects");
        let _held = claim(&home, &root).unwrap().unwrap();
        let inner = root.join("app");
        assert!(claim(&home, &inner).unwrap_err().to_string().contains("inside"));
        assert_eq!(
            std::fs::read(dir(&home, &inner).join("root")).unwrap(),
            inner.as_os_str().as_encoded_bytes()
        );
    }

    /// C7: a root that isn't UTF-8 is still found.
    #[cfg(unix)]
    #[test]
    fn a_non_utf8_root_is_checked() {
        use std::os::unix::ffi::OsStrExt;
        let (t, home) = setup();
        let root = t.path().join(std::ffi::OsStr::from_bytes(b"proj\xff"));
        let _held = claim(&home, &root).unwrap().unwrap();
        let inside = check_root(&home, &root.join("app")).unwrap_err().to_string();
        assert!(inside.contains("inside"), "{inside}");
    }

    #[test]
    fn wait_gives_up_after_the_timeout() {
        let (t, home) = setup();
        let root = t.path().join("projects");
        let e = wait(&home, &root, Duration::from_millis(150), |_| true).unwrap_err();
        assert!(e.to_string().contains("didn't start"), "{e}");
    }

    #[test]
    fn root_key_is_stable_per_path() {
        let a = dir(Path::new("/h"), Path::new("/x/projects"));
        assert_eq!(a, dir(Path::new("/h"), Path::new("/x/projects")));
        assert_ne!(a, dir(Path::new("/h"), Path::new("/x/projects2")));
    }
}
