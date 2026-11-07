//! Restart after an upgrade (L10.9 "Super restarts after upgrade"): super remembers its
//! binary as it started; once the file at that path changes (a new install) and super is
//! idle, it restarts itself as the same session, as `novacode stop` and `novacode --super`
//! would. Tasks are separate processes and keep running.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

/// Set when super ends to restart: its serve loop then runs the binary on disk again.
static RESTART: AtomicBool = AtomicBool::new(false);

/// Ask for the restart (the session is ended by the caller).
pub fn request() {
    RESTART.store(true, Ordering::SeqCst);
}

/// Whether super ended to restart.
pub fn requested() -> bool {
    RESTART.load(Ordering::SeqCst)
}

/// What tells one install of the binary from the next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ident {
    modified: Option<std::time::SystemTime>,
    len: u64,
    #[cfg(unix)]
    ino: u64,
}

/// The identity of the file at `path`, `None` when it can't be read (gone mid-install).
pub fn ident(path: &Path) -> Option<Ident> {
    let m = std::fs::metadata(path).ok()?;
    Some(Ident {
        modified: m.modified().ok(),
        len: m.len(),
        #[cfg(unix)]
        ino: std::os::unix::fs::MetadataExt::ino(&m),
    })
}

/// Super's binary as it started, and whether it changed since.
pub struct ExeWatch {
    pub path: PathBuf,
    /// The file as super started on it.
    seen: Option<Ident>,
    /// Linux names a running binary that was replaced `<path> (deleted)`.
    stale: bool,
    /// The file at the last check: a restart waits until it holds still for one check.
    last: Option<Ident>,
    /// The identity a restart was tried for: once per changed file (no restart loop).
    tried: Option<Ident>,
}

impl ExeWatch {
    /// This process's binary, from `current_exe`.
    pub fn current() -> Option<ExeWatch> {
        let exe = std::env::current_exe().ok()?;
        let (path, stale) = match exe.to_str().and_then(|s| s.strip_suffix(" (deleted)")) {
            Some(p) => (PathBuf::from(p), true),
            None => (exe, false),
        };
        let seen = ident(&path);
        Some(ExeWatch::new(path, seen, stale))
    }

    pub fn new(path: PathBuf, seen: Option<Ident>, stale: bool) -> ExeWatch {
        ExeWatch {
            path,
            seen,
            stale,
            last: seen,
            tried: None,
        }
    }

    /// Whether super restarts now: the file at its path is `now`, a readable file that
    /// differs from the one super started on and held still since the last check, and
    /// super is `idle`. True once per changed file.
    pub fn due(&mut self, now: Option<Ident>, idle: bool) -> bool {
        let still = now == self.last;
        self.last = now;
        let changed = now.is_some() && (self.stale || now != self.seen);
        if !changed || !still || !idle || self.tried == now {
            return false;
        }
        self.tried = now;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_changed_binary_restarts_super_once_when_it_holds_still_and_super_is_idle() {
        let d = tempfile::tempdir().unwrap();
        let exe = d.path().join("novacode");
        std::fs::write(&exe, "v1").unwrap();
        let mut w = ExeWatch::new(exe.clone(), ident(&exe), false);
        assert!(!w.due(ident(&exe), true), "unchanged");
        // A new install: another file at the same path.
        std::fs::remove_file(&exe).unwrap();
        std::fs::write(&exe, "v2 longer").unwrap();
        assert!(!w.due(ident(&exe), true), "it just changed: wait one check");
        assert!(!w.due(ident(&exe), false), "super is working");
        assert!(w.due(ident(&exe), true));
        assert!(!w.due(ident(&exe), true), "once per changed file");
        assert!(!w.due(None, true), "a file being replaced is not restarted on");
    }

    #[test]
    fn a_binary_already_replaced_at_start_restarts_super() {
        let d = tempfile::tempdir().unwrap();
        let exe = d.path().join("novacode");
        std::fs::write(&exe, "v2").unwrap();
        let mut w = ExeWatch::new(exe.clone(), ident(&exe), true);
        assert!(w.due(ident(&exe), true));
        assert!(!w.due(ident(&exe), true));
    }
}
