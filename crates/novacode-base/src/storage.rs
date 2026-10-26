//! Storage primitives (L0.5): atomic writes with a `.bak`, secret files,
//! single-write JSONL appends, cross-process file locks.

use serde::{Serialize, de::DeserializeOwned};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Durability {
    /// fsync the file and its directory: survives power loss.
    Durable,
    /// No fsync: survives a process crash, not a power cut. For caches.
    Fast,
}

pub fn backup_path(path: &Path) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(".bak");
    PathBuf::from(s)
}

/// Replace `path` with `bytes` so readers see either the old or the new file,
/// never a torn one. The old file is kept as `<path>.bak`.
pub fn write_atomic(path: &Path, bytes: &[u8], durability: Durability) -> io::Result<()> {
    write_impl(path, bytes, durability, None, true)
}

/// Like [`write_atomic`], but no `.bak` is made or touched: for user files,
/// where a `<path>.bak` may be the user's own.
pub fn write_atomic_no_backup(path: &Path, bytes: &[u8], durability: Durability) -> io::Result<()> {
    write_impl(path, bytes, durability, None, false)
}

/// Like [`write_atomic`], but the file is `0600` from its first byte and the
/// parent dir is made `0700`. Always durable.
pub fn write_secret(path: &Path, bytes: &[u8]) -> io::Result<()> {
    write_impl(path, bytes, Durability::Durable, Some(0o600), true)
}

fn write_impl(
    path: &Path,
    bytes: &[u8],
    durability: Durability,
    mode: Option<u32>,
    keep_backup: bool,
) -> io::Result<()> {
    // Write through a symlink to its target, so a linked config (dotfiles/stow) stays linked.
    // A dangling link or a missing file: keep `path` as given.
    let path = &crate::platform::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let dir = path
        .parent()
        .filter(|d| !d.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    fs::create_dir_all(dir)?;
    if mode.is_some() {
        crate::platform::set_mode(dir, 0o700)?;
    }
    let name = path
        .file_name()
        .ok_or_else(|| io::Error::other("path has no file name"))?;
    let tmp = dir.join(format!(
        ".{}.tmp.{}.{}",
        name.to_string_lossy(),
        std::process::id(),
        crate::new_id().simple()
    ));
    let mut opts = OpenOptions::new();
    opts.write(true).create_new(true);
    if let Some(m) = mode {
        crate::platform::set_create_mode(&mut opts, m);
    }
    let result = (|| {
        let mut f = opts.open(&tmp)?;
        f.write_all(bytes)?;
        if durability == Durability::Durable {
            f.sync_all()?;
        }
        drop(f);
        if keep_backup && path.exists() {
            let bak = backup_path(path);
            let _ = fs::remove_file(&bak);
            // Hard link keeps the old bytes without a copy; the primary path never goes missing.
            if let Err(e) = fs::hard_link(path, &bak) {
                tracing::warn!(path = %path.display(), error = %e, "could not keep .bak");
            }
        }
        fs::rename(&tmp, path)?;
        // Windows can't open a folder as a file; NTFS journals the rename itself.
        #[cfg(unix)]
        if durability == Durability::Durable {
            File::open(dir)?.sync_all()?;
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

/// Serialize as pretty JSON and [`write_atomic`].
pub fn write_json<T: Serialize>(path: &Path, value: &T, durability: Durability) -> io::Result<()> {
    let mut bytes = serde_json::to_vec_pretty(value).map_err(io::Error::other)?;
    bytes.push(b'\n');
    write_atomic(path, &bytes, durability)
}

/// Read an app-owned JSON file. Missing → `None`. If the primary is corrupt
/// and `.bak` parses, the backup is restored and returned.
pub fn read_json<T: DeserializeOwned>(path: &Path) -> io::Result<Option<T>> {
    let primary = match fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let err = match serde_json::from_slice(&primary) {
        Ok(v) => return Ok(Some(v)),
        Err(e) => e,
    };
    // `.bak` lives next to a symlink's target (see `write_impl`).
    let real = crate::platform::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let bak = backup_path(&real);
    if let Ok(b) = fs::read(&bak)
        && let Ok(v) = serde_json::from_slice(&b)
    {
        tracing::warn!(path = %path.display(), error = %err, "corrupt file, restored from .bak");
        let tmp_restore = fs::read(&bak)?;
        // Restore without overwriting .bak with the corrupt primary; remove the
        // symlink's target, not the link itself.
        let _ = fs::remove_file(&real);
        write_atomic(&real, &tmp_restore, Durability::Durable)?;
        return Ok(Some(v));
    }
    Err(io::Error::new(io::ErrorKind::InvalidData, err))
}

/// Append one line with a single `write` on an `O_APPEND` fd, so a crash or a
/// concurrent appender can't interleave or tear it.
pub fn append_line(path: &Path, line: &str) -> io::Result<()> {
    let mut buf = Vec::with_capacity(line.len() + 1);
    buf.extend_from_slice(line.as_bytes());
    buf.push(b'\n');
    let mut f = OpenOptions::new().create(true).append(true).open(path)?;
    f.write_all(&buf)
}

pub fn append_jsonl<T: Serialize>(path: &Path, value: &T) -> io::Result<()> {
    let line = serde_json::to_string(value).map_err(io::Error::other)?;
    append_line(path, &line)
}

/// Exclusive cross-process lock on `<path>.lock`, released on drop.
#[derive(Debug)]
pub struct FileLock {
    _file: File,
}

pub fn lock(path: &Path) -> io::Result<FileLock> {
    let mut s = path.as_os_str().to_owned();
    s.push(".lock");
    let lock_path = PathBuf::from(s);
    if let Some(dir) = lock_path.parent() {
        fs::create_dir_all(dir)?;
    }
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(lock_path)?;
    file.lock()?;
    Ok(FileLock { _file: file })
}

/// Like [`lock`] but returns `None` at once when another holder has it.
pub fn try_lock(path: &Path) -> io::Result<Option<FileLock>> {
    let mut s = path.as_os_str().to_owned();
    s.push(".lock");
    let lock_path = PathBuf::from(s);
    if let Some(dir) = lock_path.parent() {
        fs::create_dir_all(dir)?;
    }
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(lock_path)?;
    match file.try_lock() {
        Ok(()) => Ok(Some(FileLock { _file: file })),
        Err(fs::TryLockError::WouldBlock) => Ok(None),
        Err(fs::TryLockError::Error(e)) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn atomic_write_keeps_backup_and_no_temp_files() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("a.json");
        write_atomic(&p, b"one", Durability::Durable).unwrap();
        write_atomic(&p, b"two", Durability::Fast).unwrap();
        assert_eq!(fs::read(&p).unwrap(), b"two");
        assert_eq!(fs::read(backup_path(&p)).unwrap(), b"one");
        let names: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names.len(), 2, "temp file left behind: {names:?}");
    }

    #[test]
    fn no_backup_write_leaves_an_existing_bak_alone() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("notes");
        fs::write(&p, b"old").unwrap();
        fs::write(backup_path(&p), b"mine").unwrap();
        write_atomic_no_backup(&p, b"new", Durability::Fast).unwrap();
        assert_eq!(fs::read(&p).unwrap(), b"new");
        assert_eq!(fs::read(backup_path(&p)).unwrap(), b"mine");
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 2, "temp file left behind");
    }

    #[test]
    fn corrupt_primary_falls_back_to_backup() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("auth.json");
        write_json(&p, &serde_json::json!({"v": 1}), Durability::Durable).unwrap();
        write_json(&p, &serde_json::json!({"v": 2}), Durability::Durable).unwrap();
        fs::write(&p, b"{ torn").unwrap(); // simulate a non-atomic writer crashing
        let v: serde_json::Value = read_json(&p).unwrap().unwrap();
        assert_eq!(v["v"], 1);
        let again: serde_json::Value = read_json(&p).unwrap().unwrap();
        assert_eq!(again["v"], 1, "primary was repaired");
        assert!(
            read_json::<serde_json::Value>(&dir.path().join("none"))
                .unwrap()
                .is_none()
        );
    }

    // File symlinks on Windows need Developer Mode or admin.
    #[cfg(unix)]
    #[test]
    fn write_goes_through_a_symlink() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real");
        let home = dir.path().join("home");
        fs::create_dir_all(&real).unwrap();
        fs::create_dir_all(&home).unwrap();
        fs::write(real.join("settings.json"), b"{}").unwrap();
        let link = home.join("settings.json");
        symlink("../real/settings.json", &link).unwrap();
        write_atomic(&link, b"{\"a\":1}", Durability::Fast).unwrap();
        assert!(fs::symlink_metadata(&link).unwrap().file_type().is_symlink());
        assert_eq!(fs::read(real.join("settings.json")).unwrap(), b"{\"a\":1}");
        assert_eq!(fs::read(real.join("settings.json.bak")).unwrap(), b"{}");
        assert_eq!(fs::read_dir(&home).unwrap().count(), 1, "stray file in home/");
        assert_eq!(fs::read_dir(&real).unwrap().count(), 2, "temp file left in real/");

        // Restoring a corrupt primary from .bak keeps the link too.
        fs::write(real.join("settings.json"), b"{ torn").unwrap();
        let v: serde_json::Value = read_json(&link).unwrap().unwrap();
        assert_eq!(v, serde_json::json!({}));
        assert!(fs::symlink_metadata(&link).unwrap().file_type().is_symlink());
        assert_eq!(fs::read(real.join("settings.json")).unwrap(), b"{}");
    }

    #[cfg(unix)]
    #[test]
    fn dangling_symlink_and_missing_path_still_get_written() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let dangling = dir.path().join("d.json");
        symlink("nowhere/x.json", &dangling).unwrap();
        write_atomic(&dangling, b"1", Durability::Fast).unwrap();
        assert!(fs::symlink_metadata(&dangling).unwrap().file_type().is_file());
        assert_eq!(fs::read(&dangling).unwrap(), b"1");
        let fresh = dir.path().join("new/f.json");
        write_atomic(&fresh, b"2", Durability::Durable).unwrap();
        assert_eq!(fs::read(&fresh).unwrap(), b"2");
    }

    #[cfg(unix)]
    #[test]
    fn secret_is_0600_in_0700_dir() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("sub/auth.json");
        write_secret(&p, b"{}").unwrap();
        let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&p), 0o600);
        assert_eq!(mode(p.parent().unwrap()), 0o700);
    }

    #[test]
    fn concurrent_appends_never_interleave() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("s.jsonl");
        let line = "x".repeat(3000);
        std::thread::scope(|s| {
            for _ in 0..8 {
                s.spawn(|| (0..50).for_each(|_| append_line(&p, &line).unwrap()));
            }
        });
        let text = fs::read_to_string(&p).unwrap();
        assert_eq!(text.lines().count(), 400);
        assert!(text.lines().all(|l| l == line));
    }

    #[test]
    fn lock_is_exclusive() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("settings.json");
        let held = lock(&p).unwrap();
        let other = OpenOptions::new()
            .write(true)
            .open(dir.path().join("settings.json.lock"))
            .unwrap();
        assert!(other.try_lock().is_err());
        drop(held);
        // A process another test forks in this window holds the fd until its exec
        // (close-on-exec), so allow a short wait.
        let free = (0..100).any(|_| {
            other.try_lock().is_ok() || {
                std::thread::sleep(std::time::Duration::from_millis(10));
                false
            }
        });
        assert!(free);
    }
}
