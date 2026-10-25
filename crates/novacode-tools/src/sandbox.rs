//! Sandbox (L6.5): commands the model runs (`bash`, `process` start) run under
//! bubblewrap. The whole file system is read-only; the project, `/tmp` and the
//! tool caches are writable; credentials are hidden; the network stays on.
//! [`command`] builds every model command, so the bash and process paths can't drift.

use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// `sandbox.mode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mode {
    /// Sandbox when bwrap works; one warning when it doesn't, then unsandboxed.
    #[default]
    Auto,
    /// Refuse commands when it can't sandbox.
    On,
    Off,
}

impl Mode {
    pub fn parse(s: &str) -> Option<Mode> {
        match s {
            "auto" => Some(Mode::Auto),
            "on" => Some(Mode::On),
            "off" => Some(Mode::Off),
            _ => None,
        }
    }
}

/// What the sandbox is built from. Paths in the lists may start with `~/`;
/// relative ones are taken from the first project folder.
#[derive(Debug, Clone, Default)]
pub struct Config {
    pub mode: Mode,
    /// Writable: the project root, and a worktree's shared git folder.
    pub project: Vec<PathBuf>,
    pub home: PathBuf,
    /// novacode's own folder: its login and trust files are hidden.
    pub novacode_home: PathBuf,
    /// `$TMPDIR`, writable when set.
    pub tmpdir: Option<PathBuf>,
    /// `sandbox.allowWrite`.
    pub allow_write: Vec<String>,
    /// Hidden: the user's protected paths and `sandbox.denyRead`.
    pub deny_read: Vec<String>,
    /// Credential folders under home, the risk gate's list (`risk::credential_dirs`, L6.2).
    pub credential_dirs: Vec<String>,
}

/// Tool caches under home, writable when they exist.
const CACHES: &[&str] = &[".cache", ".cargo", ".rustup", ".npm", ".bun", "go"];
/// Token files under home, hidden when they exist.
const TOKEN_FILES: &[&str] = &[
    ".git-credentials",
    ".netrc",
    ".cargo/credentials",
    ".cargo/credentials.toml",
    ".docker/config.json",
    ".config/gh/hosts.yml",
    ".pypirc",
    ".npmrc",
];
/// novacode's login and trust files (plus `auth-*.json`).
const NOVACODE_FILES: &[&str] = &["auth.json", "mcp-auth.json", "trust.json"];

pub struct Sandbox {
    mode: Mode,
    /// Writable binds, parents first.
    write: Vec<PathBuf>,
    /// (path, is a folder): folders get an empty read-only tmpfs, files `/dev/null`.
    hide: Vec<(PathBuf, bool)>,
    program: PathBuf,
    works: OnceLock<bool>,
}

fn expand(p: &str, home: &Path, base: &Path) -> PathBuf {
    if p == "~" {
        home.to_path_buf()
    } else if let Some(rest) = p.strip_prefix("~/") {
        home.join(rest)
    } else {
        base.join(p)
    }
}

impl Sandbox {
    /// Resolves the layout once: paths that don't exist are skipped.
    pub fn new(c: Config) -> Self {
        let base = c.project.first().cloned().unwrap_or_else(|| PathBuf::from("/"));
        let mut write: Vec<PathBuf> = c
            .project
            .iter()
            .cloned()
            .chain([PathBuf::from("/tmp")])
            .chain(c.tmpdir.clone())
            .chain(CACHES.iter().map(|d| c.home.join(d)))
            .chain(c.allow_write.iter().map(|p| expand(p, &c.home, &base)))
            .filter_map(|p| std::fs::canonicalize(p).ok())
            .collect();
        write.sort();
        write.dedup();
        // Parents first, so a bind inside another one isn't covered by it.
        write.sort_by_key(|p| p.components().count());
        // The whole ~/.novacode is not hidden (skills and settings are read from it), only its logins.
        let mut cands: Vec<PathBuf> = c
            .credential_dirs
            .iter()
            .filter(|d| *d != ".novacode")
            .map(|d| c.home.join(d))
            .collect();
        cands.extend(NOVACODE_FILES.iter().map(|f| c.novacode_home.join(f)));
        if let Ok(rd) = std::fs::read_dir(&c.novacode_home) {
            let mut auth: Vec<PathBuf> = rd
                .flatten()
                .map(|e| e.path())
                .filter(|p| {
                    p.file_name()
                        .and_then(|n| n.to_str())
                        .is_some_and(|n| n.starts_with("auth-") && n.ends_with(".json"))
                })
                .collect();
            auth.sort();
            cands.extend(auth);
        }
        cands.extend(TOKEN_FILES.iter().map(|f| c.home.join(f)));
        cands.extend(c.deny_read.iter().map(|p| expand(p, &c.home, &base)));
        let mut hide: Vec<(PathBuf, bool)> = Vec::new();
        for p in cands {
            if let (Ok(real), Ok(m)) = (std::fs::canonicalize(&p), std::fs::metadata(&p))
                && !hide.iter().any(|(h, _)| *h == real)
            {
                hide.push((real, m.is_dir()));
            }
        }
        Sandbox {
            mode: c.mode,
            write,
            hide,
            program: PathBuf::from("bwrap"),
            works: OnceLock::new(),
        }
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    /// bwrap works here: checked once, then cached. In `auto` mode a failed
    /// check logs the one warning.
    pub fn available(&self) -> bool {
        *self.works.get_or_init(|| {
            let ok = std::process::Command::new(&self.program)
                .args(["--ro-bind", "/", "/", "--dev", "/dev", "--proc", "/proc"])
                .args(["--unshare-pid", "--die-with-parent", "true"])
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .is_ok_and(|s| s.success());
            if !ok && self.mode == Mode::Auto {
                tracing::warn!(
                    "sandbox: bubblewrap (bwrap) is not installed or doesn't work here; commands run unsandboxed"
                );
            }
            ok
        })
    }

    /// Commands run inside it.
    pub fn wraps(&self) -> bool {
        self.mode != Mode::Off && self.available()
    }

    /// The bwrap arguments before `--`, for a command run in `cwd`.
    pub fn args(&self, cwd: &Path) -> Vec<OsString> {
        let mut a: Vec<OsString> = [
            "--ro-bind",
            "/",
            "/",
            "--dev",
            "/dev",
            "--proc",
            "/proc",
            // Kill semantics: killing bwrap (timeout, Esc, `process stop`, novacode dying)
            // ends the pid namespace, so nothing started inside survives, even a `setsid` child.
            // No `--new-session`: the command stays in the caller's process group for killpg.
            "--unshare-pid",
            "--die-with-parent",
        ]
        .map(OsString::from)
        .into();
        for w in &self.write {
            a.extend(["--bind".into(), w.clone().into(), w.clone().into()]);
        }
        // Last, so they win inside writable folders.
        for (h, dir) in &self.hide {
            if *dir {
                a.extend(["--tmpfs".into(), h.clone().into()]);
                a.extend(["--remount-ro".into(), h.clone().into()]);
            } else {
                a.extend(["--ro-bind".into(), "/dev/null".into(), h.clone().into()]);
            }
        }
        a.extend(["--chdir".into(), cwd.as_os_str().to_owned()]);
        a
    }

    /// The line added when a sandboxed command failed on a blocked write.
    pub fn hint(&self) -> String {
        let places: Vec<String> = self.write.iter().map(|p| p.display().to_string()).collect();
        format!(
            "[sandbox: commands may write only in {}; pass `unsandboxed: true` to run this one outside it (the command judge checks such calls)]",
            places.join(", ")
        )
    }
}

/// The hint for a command's result, when it ran sandboxed, failed, and its
/// output shows a blocked write.
pub fn hint_for(sandbox: Option<&Sandbox>, failed: bool, output: &str) -> Option<String> {
    let s = sandbox.filter(|s| s.wraps())?;
    (failed && output.contains("Read-only file system")).then(|| s.hint())
}

/// Builds every model command (L6.5): `bash -c CMD`, or `bwrap <args> -- bash -c CMD`.
/// `None` runs it outside (the `unsandboxed` argument, or no sandbox). The caller
/// sets env, stdio and the process group.
pub fn command(sandbox: Option<&Sandbox>, cmd: &str, cwd: &Path) -> io::Result<tokio::process::Command> {
    let mut c = match sandbox {
        Some(s) if s.mode != Mode::Off && s.available() => {
            let mut c = tokio::process::Command::new(&s.program);
            c.args(s.args(cwd)).args(["--", "bash"]);
            c
        }
        Some(s) if s.mode == Mode::On => {
            return Err(io::Error::other(
                "the sandbox is on (`sandbox.mode`), but bubblewrap (bwrap) is not installed or doesn't work here, so the command was not run. Install bubblewrap, set `sandbox.mode` to `auto`, or pass `unsandboxed: true` for this one command",
            ));
        }
        _ => tokio::process::Command::new("bash"),
    };
    c.arg("-c").arg(cmd).current_dir(cwd);
    Ok(c)
}

/// The writable project folders of a session: the git top level of `cwd`
/// (falling back to `cwd`), plus the shared git folder of a worktree when it
/// lies outside, so commits there work.
pub fn project_dirs(cwd: &Path) -> Vec<PathBuf> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args([
            "rev-parse",
            "--path-format=absolute",
            "--show-toplevel",
            "--git-common-dir",
        ])
        .stderr(std::process::Stdio::null())
        .output()
        .ok()
        .filter(|o| o.status.success());
    let Some(out) = out else {
        return vec![cwd.to_path_buf()];
    };
    let lines: Vec<PathBuf> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(PathBuf::from)
        .collect();
    let Some(top) = lines.first().cloned() else {
        return vec![cwd.to_path_buf()];
    };
    let mut dirs = vec![top.clone()];
    if let Some(common) = lines.get(1)
        && !common.starts_with(&top)
    {
        dirs.push(common.clone());
    }
    dirs
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn strs(a: &[OsString]) -> Vec<String> {
        a.iter().map(|s| s.to_string_lossy().into_owned()).collect()
    }

    /// Fake home with some of the listed places present.
    fn setup() -> (tempfile::TempDir, Config) {
        let d = tempfile::tempdir().unwrap();
        let home = d.path().join("home");
        let proj = d.path().join("proj");
        for dir in [".ssh", ".cache", ".novacode", ".config/gh", "extra", "proj-secret"] {
            std::fs::create_dir_all(home.join(dir)).unwrap();
        }
        std::fs::create_dir_all(proj.join("sub")).unwrap();
        for f in [
            ".novacode/auth.json",
            ".novacode/auth-work.json",
            ".novacode/settings.json",
            ".netrc",
            ".config/gh/hosts.yml",
        ] {
            std::fs::write(home.join(f), "x").unwrap();
        }
        std::fs::write(proj.join(".env.secret"), "x").unwrap();
        let c = Config {
            mode: Mode::On,
            project: vec![proj.clone()],
            home: home.clone(),
            novacode_home: home.join(".novacode"),
            tmpdir: None,
            allow_write: vec!["~/extra".into(), "~/missing".into()],
            deny_read: vec![".env.secret".into(), "~/nope".into()],
            credential_dirs: vec![".ssh".into(), ".aws".into(), ".novacode".into()],
        };
        (d, c)
    }

    #[test]
    fn layout_table() {
        let (d, c) = setup();
        let root = std::fs::canonicalize(d.path()).unwrap();
        let r = |p: &str| root.join(p).display().to_string();
        let cwd = root.join("proj/sub");
        let a = strs(&Sandbox::new(c).args(&cwd));
        let pairs: Vec<(String, String)> = a
            .iter()
            .enumerate()
            .filter(|(_, s)| ["--bind", "--tmpfs", "--chdir"].contains(&s.as_str()) || *s == "/dev/null")
            .map(|(i, s)| (s.clone(), a[i + 1].clone()))
            .collect();
        let want: Vec<(String, String)> = [
            // Writable, parents first; missing ones (`~/missing`, `~/.cargo`, …) skipped.
            ("--bind", "/tmp".to_string()),
            ("--bind", r("proj")),
            ("--bind", r("home/.cache")),
            ("--bind", r("home/extra")),
            // Hidden: folders get a tmpfs, files /dev/null; `.aws` and `~/nope` don't exist.
            ("--tmpfs", r("home/.ssh")),
            ("/dev/null", r("home/.novacode/auth.json")),
            ("/dev/null", r("home/.novacode/auth-work.json")),
            ("/dev/null", r("home/.netrc")),
            ("/dev/null", r("home/.config/gh/hosts.yml")),
            ("/dev/null", r("proj/.env.secret")),
            ("--chdir", cwd.display().to_string()),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect();
        assert_eq!(pairs, want, "{a:?}");
        assert_eq!(&a[..5], ["--ro-bind", "/", "/", "--dev", "/dev"]);
        assert!(a.contains(&"--unshare-pid".into()) && a.contains(&"--die-with-parent".into()));
        assert!(!a.contains(&"--new-session".into()), "killpg must reach the command");
        // The whole ~/.novacode stays readable (settings, skills).
        assert!(!a.contains(&r("home/.novacode")));
    }

    #[test]
    fn modes_and_a_missing_bwrap() {
        let (_d, c) = setup();
        let cwd = c.project[0].clone();
        let mut s = Sandbox::new(c.clone());
        s.program = "/nonexistent/bwrap".into();
        let e = command(Some(&s), "true", &cwd).unwrap_err();
        assert!(e.to_string().contains("not installed"), "{e}");
        assert!(!s.wraps());
        for mode in [Mode::Auto, Mode::Off] {
            let mut s = Sandbox::new(Config { mode, ..c.clone() });
            s.program = "/nonexistent/bwrap".into();
            let cmd = command(Some(&s), "true", &cwd).unwrap();
            assert_eq!(cmd.as_std().get_program(), "bash", "{mode:?} runs unsandboxed");
        }
        assert_eq!(Mode::parse("on"), Some(Mode::On));
        assert_eq!(Mode::parse("yes"), None);
        assert!(
            hint_for(Some(&s), true, "Read-only file system").is_none(),
            "not sandboxed"
        );
    }

    /// Processes whose command line is exactly `sleep <secs>`.
    fn sleeping(secs: &str) -> usize {
        let want = format!("sleep\0{secs}\0");
        std::fs::read_dir("/proc")
            .unwrap()
            .flatten()
            .filter(|e| std::fs::read(e.path().join("cmdline")).is_ok_and(|c| c == want.as_bytes()))
            .count()
    }

    /// Real runs through bwrap; skipped with a note where it doesn't work.
    #[tokio::test]
    async fn real_runs() {
        use crate::Tool;
        use crate::testutil::{ctx, text};
        use serde_json::json;
        // Under the real home, not /tmp: /tmp is writable inside the sandbox.
        let base = std::env::var_os("HOME").map(PathBuf::from).unwrap();
        let d = tempfile::Builder::new()
            .prefix(".novacode-sandbox-test")
            .tempdir_in(&base)
            .unwrap();
        let root = std::fs::canonicalize(d.path()).unwrap();
        let (home, proj) = (root.join("home"), root.join("proj"));
        std::fs::create_dir_all(home.join(".ssh")).unwrap();
        std::fs::create_dir_all(&proj).unwrap();
        std::fs::write(home.join(".ssh/id_test"), "KEY").unwrap();
        let sb = Arc::new(Sandbox::new(Config {
            mode: Mode::On,
            project: vec![proj.clone()],
            home: home.clone(),
            novacode_home: home.join(".novacode"),
            credential_dirs: vec![".ssh".into()],
            ..Config::default()
        }));
        if !sb.available() {
            eprintln!("note: real_runs skipped, bwrap is not available here");
            return;
        }
        let mut c = ctx(&proj);
        c.env = Arc::new(crate::SandboxedEnv(sb.clone()));
        let run = |cmd: String, extra: serde_json::Value| {
            let mut args = json!({ "command": cmd });
            if let (Some(a), Some(e)) = (args.as_object_mut(), extra.as_object()) {
                a.extend(e.clone());
            }
            args
        };
        // Writes in the project work; the cwd is the same inside.
        let o = crate::bash::Bash
            .execute(run("echo hi > made.txt && pwd".into(), json!({})), &c)
            .await;
        assert!(!o.is_error, "{}", text(&o));
        assert_eq!(text(&o), proj.display().to_string());
        assert_eq!(std::fs::read_to_string(proj.join("made.txt")).unwrap(), "hi\n");
        // ~/.ssh is empty inside.
        let ssh = home.join(".ssh");
        let o = crate::bash::Bash
            .execute(run(format!("ls -A {}; echo end", ssh.display()), json!({})), &c)
            .await;
        assert_eq!(text(&o), "end");
        // Writing outside fails, with the hint.
        let outside = home.join("out.txt");
        let o = crate::bash::Bash
            .execute(run(format!("echo x > {}", outside.display()), json!({})), &c)
            .await;
        let t = text(&o);
        assert!(
            o.is_error && t.contains("Read-only file system") && t.contains("unsandboxed: true"),
            "{t}"
        );
        assert!(t.contains(&proj.display().to_string()), "{t}");
        assert!(!outside.exists());
        // `unsandboxed` bypasses it, and the result says so.
        let o = crate::bash::Bash
            .execute(
                run(format!("cat {}/id_test", ssh.display()), json!({"unsandboxed": true})),
                &c,
            )
            .await;
        assert_eq!(text(&o), "KEY\n[ran outside the sandbox]");
        // A timeout kills everything the command started inside.
        let o = crate::bash::Bash
            .execute(run("sleep 381 & sleep 381".into(), json!({"timeout": 1})), &c)
            .await;
        assert!(o.is_error && text(&o).contains("timeout"), "{}", text(&o));
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        assert_eq!(sleeping("381"), 0, "timeout left a sleep running");
        // Through the process manager: `process stop` ends the whole tree too.
        let pm = crate::process::ProcessManager::new(&root.join("procs"), "s");
        let id = pm
            .start(
                "s",
                "sleep 382 & setsid sleep 382 & sleep 382",
                &proj,
                &[],
                true,
                Some(&sb),
            )
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        assert_eq!(sleeping("382"), 3);
        pm.stop(&id).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        assert_eq!(sleeping("382"), 0, "process stop left a sleep running");
    }

    #[test]
    fn project_dirs_fall_back_to_cwd() {
        let d = tempfile::tempdir().unwrap();
        assert_eq!(project_dirs(d.path()), vec![d.path().to_path_buf()]);
    }
}
