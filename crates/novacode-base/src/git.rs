//! Every git command novacode runs itself starts here (L6.5), so planted repo
//! config can't run programs through novacode. Commands the model runs go
//! through bash and the sandbox instead.
//!
//! The host git pin: a sandboxed command can write `.git/commondir`, which
//! moves git's config and hooks to a folder of its choice. So each project's
//! common git dir is resolved once, before any model command runs ([`pin`],
//! [`pin_repo`]), and every git command novacode runs in that project gets
//! `GIT_COMMON_DIR` set to it ([`command_in`]): a `commondir` planted later is
//! ignored.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;

/// (work tree root, its common git dir), both absolute.
static PINS: Mutex<Vec<(PathBuf, PathBuf)>> = Mutex::new(Vec::new());

/// `git -c core.fsmonitor=false`: a `core.fsmonitor` from a repo's config
/// (or a planted `commondir` or submodule config) would run a program on
/// every status-style read. Reads of status and diffs also pass
/// `--ignore-submodules=all`, so submodule config isn't consulted.
/// Without a folder: for commands that don't run in a project (`clone`,
/// `init --bare`); use [`command_in`] for the rest.
pub fn command() -> Command {
    let mut c = Command::new("git");
    c.args(["-c", "core.fsmonitor=false"]);
    c
}

/// [`command`] plus `-c core.hooksPath=/dev/null`, for novacode's own
/// bookkeeping (checkpoints): no hook runs. Never for commands the user asked
/// for, whose hooks are theirs to run.
pub fn bookkeeping() -> Command {
    let mut c = command();
    c.args(["-c", "core.hooksPath=/dev/null"]);
    c
}

/// [`command`] run in `dir`, with `GIT_COMMON_DIR` set to the pinned common
/// git dir when `dir` is inside a pinned work tree (the deepest one wins).
/// Outside every pinned tree it is [`command`] in `dir`, unchanged.
pub fn command_in(dir: &Path) -> Command {
    in_dir(command(), dir)
}

/// [`bookkeeping`] run in `dir`, pinned like [`command_in`].
pub fn bookkeeping_in(dir: &Path) -> Command {
    in_dir(bookkeeping(), dir)
}

fn in_dir(mut c: Command, dir: &Path) -> Command {
    c.current_dir(dir);
    if let Some(common) = pinned(dir) {
        c.env("GIT_COMMON_DIR", common);
    }
    c
}

/// Pins `root`'s common git dir to `common_dir`, replacing an earlier pin of it.
pub fn pin(root: &Path, common_dir: &Path) {
    let root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let mut pins = PINS.lock().unwrap();
    pins.retain(|(r, _)| *r != root);
    pins.push((root, common_dir.to_path_buf()));
}

/// Resolves the work tree and common git dir of `dir` and pins them; `None`
/// outside a work tree. Call it before any model command runs there: it reads
/// `commondir`. A folder already pinned keeps its pinned dir.
pub fn pin_repo(dir: &Path) -> Option<PathBuf> {
    let o = command_in(dir)
        .args([
            "rev-parse",
            "--path-format=absolute",
            "--show-toplevel",
            "--git-common-dir",
        ])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()
        .filter(|o| o.status.success())?;
    let out = String::from_utf8_lossy(&o.stdout);
    let mut lines = out.lines();
    let (top, common) = (PathBuf::from(lines.next()?), PathBuf::from(lines.next()?));
    pin(&top, &common);
    Some(common)
}

/// The pinned common git dir for `dir`: the deepest pinned root holding it.
pub fn pinned(dir: &Path) -> Option<PathBuf> {
    let dir = std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
    PINS.lock()
        .unwrap()
        .iter()
        .filter(|(r, _)| dir.starts_with(r))
        .max_by_key(|(r, _)| r.components().count())
        .map(|(_, c)| c.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(c: &Command) -> Vec<String> {
        c.get_args().map(|a| a.to_string_lossy().into_owned()).collect()
    }

    fn env(c: &Command, k: &str) -> Option<PathBuf> {
        c.get_envs()
            .find(|(n, _)| *n == k)
            .and_then(|(_, v)| v.map(PathBuf::from))
    }

    #[test]
    fn flags_are_added() {
        let c = command();
        assert_eq!(c.get_program(), "git");
        assert_eq!(args(&c), ["-c", "core.fsmonitor=false"]);
        assert_eq!(
            args(&bookkeeping()),
            ["-c", "core.fsmonitor=false", "-c", "core.hooksPath=/dev/null"]
        );
    }

    /// `git` in `dir` with test identity; false when it fails.
    fn git(dir: &Path, a: &[&str]) -> bool {
        Command::new("git")
            .args([
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(a)
            .current_dir(dir)
            .output()
            .is_ok_and(|o| o.status.success())
    }

    /// A repo whose config sets `core.fsmonitor`: status through the helper
    /// doesn't run it.
    #[test]
    fn planted_fsmonitor_does_not_run() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path();
        if !git(p, &["init", "-q"]) {
            eprintln!("note: git missing, planted_fsmonitor_does_not_run skipped");
            return;
        }
        let mark = p.join("PWNED");
        let hook = format!("touch {}", mark.display());
        assert!(git(p, &["config", "core.fsmonitor", &hook]));
        let o = command_in(p)
            .args(["status", "--porcelain", "--ignore-submodules=all"])
            .output()
            .unwrap();
        assert!(o.status.success());
        assert!(!mark.exists(), "core.fsmonitor ran");
    }

    /// Which pin a folder gets: the deepest root holding it; none outside.
    #[test]
    fn pin_table() {
        let d = tempfile::tempdir().unwrap();
        let r = std::fs::canonicalize(d.path()).unwrap();
        for sub in ["a/b/c", "a/wt/x", "ab", "other"] {
            std::fs::create_dir_all(r.join(sub)).unwrap();
        }
        pin(&r.join("a"), Path::new("/pinned/a/.git"));
        pin(&r.join("a/wt"), Path::new("/pinned/wt"));
        // Pinning again replaces.
        pin(&r.join("a/wt"), Path::new("/pinned/a/.git"));
        for (dir, want) in [
            ("a", Some("/pinned/a/.git")),
            ("a/b/c", Some("/pinned/a/.git")),
            ("a/wt/x", Some("/pinned/a/.git")),
            ("ab", None),
            ("other", None),
        ] {
            let c = command_in(&r.join(dir));
            assert_eq!(env(&c, "GIT_COMMON_DIR"), want.map(PathBuf::from), "{dir}");
            assert_eq!(c.get_current_dir(), Some(r.join(dir).as_path()));
        }
        pin(&r.join("a/wt"), Path::new("/pinned/wt"));
        assert_eq!(pinned(&r.join("a/wt/x")), Some(PathBuf::from("/pinned/wt")));
        assert_eq!(pinned(&r.join("a/b")), Some(PathBuf::from("/pinned/a/.git")));
        assert_eq!(env(&command(), "GIT_COMMON_DIR"), None);
    }

    /// The reviewer's attack: a planted `commondir` pointing at a folder whose
    /// config defines a clean filter, and `.gitattributes` using it. The
    /// checkpoint's git commands through the pinned helper don't run it, and
    /// `worktree add` doesn't run a `post-checkout` hook planted there.
    #[test]
    fn planted_commondir_is_ignored() {
        let d = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(d.path()).unwrap();
        let p = root.join("proj");
        std::fs::create_dir_all(&p).unwrap();
        if !git(&p, &["init", "-q"]) || !git(&p, &["commit", "-q", "--allow-empty", "-m", "x"]) {
            eprintln!("note: git missing, planted_commondir_is_ignored skipped");
            return;
        }
        std::fs::write(p.join("f.txt"), "a\n").unwrap();
        assert!(git(&p, &["add", "f.txt"]));
        assert!(git(&p, &["commit", "-qm", "f"]));
        // Resolved at session start, before anything is planted.
        assert_eq!(pin_repo(&p), Some(p.join(".git")));
        // Worktrees work pinned: their common dir is the main repo's `.git`.
        let wt = root.join("wt");
        let o = command_in(&p)
            .args(["worktree", "add", "-q", "-b", "wt"])
            .arg(&wt)
            .output()
            .unwrap();
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
        pin(&wt, &p.join(".git"));
        let o = command_in(&wt)
            .args(["rev-parse", "--abbrev-ref", "HEAD"])
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&o.stdout).trim(), "wt");
        assert!(
            command_in(&wt)
                .args(["status", "--porcelain"])
                .status()
                .unwrap()
                .success()
        );
        // Planted later: a copy of the git dir with a filter and a hook.
        let evil = root.join("evil");
        assert!(
            Command::new("cp")
                .arg("-r")
                .arg(p.join(".git"))
                .arg(&evil)
                .status()
                .unwrap()
                .success()
        );
        let mark = root.join("PWN");
        let hook_mark = root.join("HOOK");
        let mut config = std::fs::read_to_string(evil.join("config")).unwrap();
        config.push_str(&format!("[filter \"x\"]\n\tclean = touch {}\n", mark.display()));
        std::fs::write(evil.join("config"), config).unwrap();
        let hook = evil.join("hooks/post-checkout");
        std::fs::write(&hook, format!("#!/bin/sh\ntouch {}\n", hook_mark.display())).unwrap();
        assert!(Command::new("chmod").arg("+x").arg(&hook).status().unwrap().success());
        std::fs::write(p.join(".git/commondir"), format!("{}\n", evil.display())).unwrap();
        std::fs::write(p.join(".gitattributes"), "* filter=x\n").unwrap();
        std::fs::write(p.join("f.txt"), "b\n").unwrap();
        std::fs::write(p.join("new.txt"), "n\n").unwrap();
        // Unpinned git follows the planted `commondir` (the attack works).
        let other = root.join("other");
        let o = command()
            .current_dir(&p)
            .args(["rev-parse", "--path-format=absolute", "--git-path", "hooks"])
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&o.stdout).trim(),
            evil.join("hooks").display().to_string()
        );
        // The checkpoint's commands, through a temporary index.
        let idx = root.join("tmp-index");
        let run = |a: &[&str]| {
            let o = bookkeeping_in(&p).env("GIT_INDEX_FILE", &idx).args(a).output().unwrap();
            assert!(o.status.success(), "{a:?}: {}", String::from_utf8_lossy(&o.stderr));
        };
        run(&["add", "-u"]);
        run(&["hash-object", "-w", "new.txt"]);
        run(&["add", "--", "new.txt"]);
        run(&["write-tree"]);
        assert!(!mark.exists(), "the planted clean filter ran");
        let o = command_in(&p)
            .args(["rev-parse", "--path-format=absolute", "--git-path", "hooks"])
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&o.stdout).trim(),
            p.join(".git/hooks").display().to_string()
        );
        // `worktree add` (enter_worktree): no planted hook. (Refs still follow a
        // planted `commondir`, git reads it for them directly; no program runs from refs.)
        let o = command_in(&p)
            .args(["worktree", "add", "-q", "-b", "w"])
            .arg(&other)
            .output()
            .unwrap();
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
        assert!(!hook_mark.exists(), "the planted post-checkout hook ran");
        assert!(!mark.exists(), "the planted clean filter ran");
    }
}
