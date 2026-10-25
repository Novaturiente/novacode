//! Every git command novacode runs itself starts here (L6.5), so planted repo
//! config can't run programs through novacode. Commands the model runs go
//! through bash and the sandbox instead.

use std::process::Command;

/// `git -c core.fsmonitor=false`: a `core.fsmonitor` from a repo's config
/// (or a planted `commondir` or submodule config) would run a program on
/// every status-style read. Reads of status and diffs also pass
/// `--ignore-submodules=all`, so submodule config isn't consulted.
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

#[cfg(test)]
mod tests {
    use super::*;

    fn args(c: &Command) -> Vec<String> {
        c.get_args().map(|a| a.to_string_lossy().into_owned()).collect()
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

    /// A repo whose config sets `core.fsmonitor`: status through the helper
    /// doesn't run it.
    #[test]
    fn planted_fsmonitor_does_not_run() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path();
        let ok = Command::new("git").args(["init", "-q"]).current_dir(p).status();
        if !ok.is_ok_and(|s| s.success()) {
            eprintln!("note: git missing, planted_fsmonitor_does_not_run skipped");
            return;
        }
        let mark = p.join("PWNED");
        let hook = format!("touch {}", mark.display());
        assert!(
            Command::new("git")
                .args(["config", "core.fsmonitor", &hook])
                .current_dir(p)
                .status()
                .unwrap()
                .success()
        );
        let o = command()
            .args(["status", "--porcelain", "--ignore-submodules=all"])
            .current_dir(p)
            .output()
            .unwrap();
        assert!(o.status.success());
        assert!(!mark.exists(), "core.fsmonitor ran");
    }
}
