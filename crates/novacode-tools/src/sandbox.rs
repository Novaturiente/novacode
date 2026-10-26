//! Sandbox (L6.5): commands the model runs (`bash`, `process` start) run under
//! bubblewrap. The whole file system is read-only; the project, `/tmp` and the
//! tool caches are writable; credentials are hidden; the network stays on.
//! Executable state inside writable places (git config and hooks, tool binaries)
//! stays read-only, unix sockets are blocked by a seccomp filter, and secrets
//! are dropped from the environment.
//! [`command`] builds every model command, so the bash and process paths can't drift.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

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
    /// The git folder and the shared git folder of the project ([`git_dirs`]):
    /// their `config` and `hooks` stay read-only.
    pub git_dirs: Vec<PathBuf>,
    /// Environment variables holding provider keys, dropped from sandboxed commands.
    pub secret_env: Vec<String>,
    /// `$HISTFILE`, hidden when it exists (`~/` and relative paths from home).
    pub histfile: Option<PathBuf>,
}

/// Download caches under home, writable when they exist (none is created).
/// Only these: shells and editors source files they keep elsewhere in
/// `~/.cache`, `~/.bun` and `~/.npm` at start. The rest of `~/.cargo`
/// (binaries, `config.toml`, `env`) stays read-only, also where it doesn't exist yet.
const CACHES: &[&str] = &[
    ".cache/pip",
    ".cache/uv",
    ".cache/go-build",
    ".cache/pnpm",
    ".cache/yarn",
    ".cache/sccache",
    ".cache/deno",
    ".cache/typescript",
    ".cache/huggingface",
    ".npm/_cacache",
    ".bun/install/cache",
    ".cargo/registry",
    ".cargo/git",
    "go/pkg/mod",
];
/// Private data under home, hidden when it exists: shell and REPL history,
/// browser and mail profiles, the keyring, Flatpak app data.
const PRIVATE: &[&str] = &[
    ".zsh_history",
    ".zhistory",
    ".histfile",
    ".local/state/zsh",
    ".bash_history",
    ".local/share/fish/fish_history",
    ".local/share/atuin",
    ".python_history",
    ".node_repl_history",
    ".psql_history",
    ".sqlite_history",
    ".lesshst",
    ".var/app",
    ".mozilla",
    ".config/google-chrome",
    ".config/chromium",
    ".config/BraveSoftware",
    ".thunderbird",
    ".local/share/keyrings",
];
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
    ".claude/.credentials.json",
    ".codex/auth.json",
    ".config/hub",
    ".azure",
    ".terraform.d/credentials.tfrc.json",
    ".pgpass",
    ".vault-token",
    ".config/rclone",
];
/// Tool binaries and config under home, read-only inside the writable caches:
/// a planted shim or `rustc-wrapper` would run outside the sandbox later.
const RO_HOME: &[&str] = &[".rustup", ".bun/bin", "go/bin"];
/// Project files novacode and other agents run or load, read-only when they exist.
const RO_PROJECT: &[&str] = &[".novacode", ".claude", ".mcp.json"];
/// Files in a git folder that git runs commands from (`core.fsmonitor`, hooks).
const RO_GIT: &[&str] = &["config", "config.worktree", "hooks"];
/// How deep submodule git folders (`<gitdir>/modules/**`) are searched.
const MODULES_DEPTH: usize = 8;
/// Parts of variable names that mark a secret (upper-cased match).
const SECRET_PARTS: &[&str] = &[
    "TOKEN",
    "SECRET",
    "PASSWORD",
    "PASSWD",
    "API_KEY",
    "APIKEY",
    "ACCESS_KEY",
    "PRIVATE_KEY",
    "CREDENTIAL",
];
/// Name endings that mark a secret (`DEPLOY_KEY`, `SIGNING_KEYS`).
const SECRET_ENDS: &[&str] = &["_KEY", "_KEYS"];
/// Underscore-separated words that mark a secret (`GH_AUTH`, `AUTH_TOKEN`;
/// not `GIT_AUTHOR_NAME`).
const SECRET_WORDS: &[&str] = &["AUTH"];
/// Variables that point at agents and desktop services.
const SECRET_NAMES: &[&str] = &[
    "SSH_AUTH_SOCK",
    "GPG_AGENT_INFO",
    "DBUS_SESSION_BUS_ADDRESS",
    "DOCKER_HOST",
];
/// novacode's login and trust files (plus `auth-*.json`).
const NOVACODE_FILES: &[&str] = &["auth.json", "mcp-auth.json", "trust.json"];

pub struct Sandbox {
    mode: Mode,
    /// Writable binds, parents first.
    write: Vec<PathBuf>,
    /// Git folders bound onto themselves, parents first, so they are mount
    /// points: renaming or removing one fails (`EBUSY`), and a copy can't take
    /// its place. Every folder from the git folder down to each submodule git
    /// folder, `modules` folders included.
    pin: Vec<PathBuf>,
    /// What the git change check looks at; `None` outside git.
    git: Option<Arc<GitProbe>>,
    /// A protected path that is a symlink inside a writable place: a bind
    /// can't protect it (the link stays replaceable), so there is no sandbox.
    symlink: Option<PathBuf>,
    /// Read-only binds inside the writable ones.
    ro: Vec<PathBuf>,
    /// (path, is a folder): folders get an empty read-only tmpfs, files `/dev/null`.
    hide: Vec<(PathBuf, bool)>,
    secret_env: Vec<String>,
    program: PathBuf,
    works: OnceLock<bool>,
    /// The seccomp program in a memfd, written once; `None` on another CPU architecture.
    filter: OnceLock<Option<File>>,
}

/// The first symlink on `p` (itself or a folder above it) that sits in a writable
/// place, when `p` exists: a command could swap that link for its own.
fn link_in(p: &Path, write: &[PathBuf]) -> Option<PathBuf> {
    p.symlink_metadata().ok()?;
    p.ancestors()
        .filter(|a| a.symlink_metadata().is_ok_and(|m| m.file_type().is_symlink()))
        .find(|a| {
            a.parent()
                .and_then(|d| std::fs::canonicalize(d).ok())
                .is_some_and(|d| write.iter().any(|w| d.starts_with(w)))
        })
        .map(Path::to_path_buf)
}

/// Submodule git folders under `dir` (`<gitdir>/modules`): a folder with a
/// `HEAD` is one (its own `modules` holds nested ones); other folders are parts
/// of a submodule name with slashes. Symlinks are not followed.
fn submodule_dirs(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
    if depth >= MODULES_DEPTH {
        return;
    }
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    let mut dirs: Vec<PathBuf> = rd
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .map(|e| e.path())
        .collect();
    dirs.sort();
    for d in dirs {
        if d.join("HEAD").is_file() {
            submodule_dirs(&d.join("modules"), depth + 1, out);
            out.push(d);
        } else {
            submodule_dirs(&d, depth + 1, out);
        }
    }
}

/// Closes every file descriptor above 2 that novacode inherited from its parent
/// (a shell can pass a pipe on fd 13), so none reaches the commands it runs.
/// Call it first in `main`, while nothing else is open. Closing, not marking
/// close-on-exec: setting the flag on a bare number needs `unsafe`, which the
/// workspace forbids, and novacode never uses an inherited descriptor.
pub fn close_inherited_fds() {
    #[cfg(unix)]
    close_fds(&open_fds());
}

/// Where a child opens this process's descriptor `f`.
#[cfg(unix)]
fn fd_path(f: &File) -> String {
    use std::os::fd::AsRawFd;
    format!("/proc/{}/fd/{}", std::process::id(), f.as_raw_fd())
}

// Never reached: `memfd` fails outside Linux.
#[cfg(windows)]
fn fd_path(_: &File) -> String {
    String::new()
}

/// The descriptors above 2 open in this process.
#[cfg(unix)]
fn open_fds() -> Vec<i32> {
    let Ok(rd) = std::fs::read_dir("/proc/self/fd") else {
        return vec![];
    };
    // Collected first: the listing's own descriptor is closed when `rd` drops.
    rd.flatten()
        .filter_map(|e| e.file_name().to_str()?.parse().ok())
        .filter(|&fd| fd > 2)
        .collect()
}

#[cfg(unix)]
fn close_fds(fds: &[i32]) {
    for &fd in fds {
        // EBADF for the listing's own descriptor, already closed.
        let _ = nix::unistd::close(fd);
    }
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
        cands.extend(PRIVATE.iter().map(|f| c.home.join(f)));
        cands.extend(
            c.histfile
                .iter()
                .map(|h| expand(&h.to_string_lossy(), &c.home, &c.home)),
        );
        cands.extend(c.deny_read.iter().map(|p| expand(p, &c.home, &base)));
        let mut symlink = cands.iter().find_map(|p| link_in(p, &write));
        let mut hide: Vec<(PathBuf, bool)> = Vec::new();
        for p in cands {
            if let (Ok(real), Ok(m)) = (std::fs::canonicalize(&p), std::fs::metadata(&p))
                && !hide.iter().any(|(h, _)| *h == real)
            {
                hide.push((real, m.is_dir()));
            }
        }
        // Submodule git folders get the same protection as the project's.
        let mut git_dirs = c.git_dirs.clone();
        for g in &c.git_dirs {
            submodule_dirs(&g.join("modules"), 0, &mut git_dirs);
        }
        // The git folders, their `modules` folders, and every folder down to each submodule git folder.
        let mut pin_cands: Vec<PathBuf> = Vec::new();
        for g in &c.git_dirs {
            pin_cands.extend([g.clone(), g.join("modules")]);
            for s in git_dirs.iter().filter(|s| s.starts_with(g) && *s != g) {
                pin_cands.extend(s.ancestors().take_while(|a| *a != g).map(Path::to_path_buf));
            }
        }
        let mut pin: Vec<PathBuf> = Vec::new();
        for p in pin_cands.iter().filter_map(|g| std::fs::canonicalize(g).ok()) {
            if p.is_dir() && !write.contains(&p) && write.iter().any(|w| p.starts_with(w)) && !pin.contains(&p) {
                pin.push(p);
            }
        }
        pin.sort_by_key(|p| p.components().count());
        let mut ro_cands: Vec<PathBuf> = git_dirs
            .iter()
            .flat_map(|g| RO_GIT.iter().map(move |f| g.join(f)))
            .chain(RO_PROJECT.iter().map(|f| base.join(f)))
            .chain(RO_HOME.iter().map(|f| c.home.join(f)))
            .collect();
        // Linked worktrees' own config (`extensions.worktreeConfig`).
        for g in &c.git_dirs {
            if let Ok(rd) = std::fs::read_dir(g.join("worktrees")) {
                let mut wt: Vec<PathBuf> = rd.flatten().map(|e| e.path().join("config.worktree")).collect();
                wt.sort();
                ro_cands.extend(wt);
            }
        }
        // A worktree's `.git` file says where its git folder is.
        if base.join(".git").is_file() {
            ro_cands.push(base.join(".git"));
        }
        symlink = symlink.or_else(|| ro_cands.iter().chain(&c.git_dirs).find_map(|p| link_in(p, &write)));
        let mut ro: Vec<PathBuf> = Vec::new();
        for p in ro_cands {
            if let Ok(real) = std::fs::canonicalize(&p)
                && !ro.contains(&real)
            {
                ro.push(real);
            }
        }
        let git = (!c.git_dirs.is_empty()).then(|| {
            Arc::new(GitProbe {
                top: base.clone(),
                git_dirs: c.git_dirs.clone(),
                links: Mutex::new(None),
                last: Mutex::new(None),
            })
        });
        Sandbox {
            mode: c.mode,
            write,
            pin,
            git,
            symlink,
            ro,
            hide,
            secret_env: c.secret_env,
            program: PathBuf::from("bwrap"),
            works: OnceLock::new(),
            filter: OnceLock::new(),
        }
    }

    /// The start of every bwrap command line. With the seccomp filter it goes
    /// through `sh`, which opens the program on fd 3 for `bwrap --seccomp 3`
    /// (passing an fd to a child directly needs `unsafe`, which the workspace forbids).
    /// Each command opens it afresh: bwrap reads the fd to its end.
    fn launcher(&self) -> Vec<OsString> {
        match self.filter_path() {
            Some(path) => vec![
                "sh".into(),
                "-c".into(),
                format!("exec \"$0\" --seccomp 3 \"$@\" 3<{path}").into(),
                self.program.clone().into(),
            ],
            None => vec![self.program.clone().into()],
        }
    }

    /// Where children open the seccomp program (novacode's memfd, written once),
    /// or `None` without a filter for this CPU or OS.
    fn filter_path(&self) -> Option<String> {
        let memfd = self.filter.get_or_init(|| {
            let made = seccomp_program().map(|bytes| -> io::Result<File> {
                let mut f = memfd()?;
                f.write_all(&bytes)?;
                Ok(f)
            });
            match made {
                Some(Ok(f)) => Some(f),
                Some(Err(e)) => {
                    tracing::warn!(error = %e, "sandbox: could not write the seccomp filter; unix sockets stay reachable");
                    None
                }
                None => {
                    tracing::warn!(
                        "sandbox: no seccomp filter for this CPU architecture; unix sockets stay reachable inside the sandbox"
                    );
                    None
                }
            }
        });
        memfd.as_ref().map(fd_path)
    }

    /// The variable is passed into sandboxed commands (not a secret).
    fn keeps_env(&self, name: &OsStr) -> bool {
        let n = name.to_string_lossy().to_ascii_uppercase();
        !(SECRET_NAMES.contains(&n.as_str())
            || SECRET_PARTS.iter().any(|p| n.contains(p))
            || SECRET_ENDS.iter().any(|e| n.ends_with(e))
            || n.split('_').any(|w| SECRET_WORDS.contains(&w))
            || self.secret_env.iter().any(|s| s.eq_ignore_ascii_case(&n)))
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    /// The protected path that is a symlink in a writable place, which makes
    /// the sandbox unavailable for this session.
    pub fn symlink(&self) -> Option<&Path> {
        self.symlink.as_deref()
    }

    /// bwrap works here: checked once, then cached. In `auto` mode a failed
    /// check logs the one warning; a protected symlink logs it in both modes.
    pub fn available(&self) -> bool {
        *self.works.get_or_init(|| {
            // L11.14: no sandbox on Windows (bubblewrap is Linux-only).
            if cfg!(windows) {
                if self.mode == Mode::Auto {
                    tracing::warn!("sandbox: there is no sandbox on Windows; commands run unsandboxed, the approval policy still applies");
                }
                return false;
            }
            if let Some(p) = &self.symlink {
                let then = if self.mode == Mode::On {
                    "commands are refused"
                } else {
                    "commands run unsandboxed"
                };
                tracing::warn!(
                    "sandbox: {} is a symlink in a writable place, so the sandbox can't protect it; {then}",
                    p.display()
                );
                return false;
            }
            // Probed with the filter, as commands run: a kernel without seccomp counts as no sandbox.
            let l = self.launcher();
            let ok = std::process::Command::new(&l[0])
                .args(&l[1..])
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
            "--unshare-pid",
            "--die-with-parent",
            // A new session without a terminal: commands can't open `/dev/tty`
            // (clipboard and reply-injection escapes reach the user's terminal).
            "--new-session",
        ]
        .map(OsString::from)
        .into();
        // A path deleted since the start is skipped: bwrap would fail on it, or
        // create it on the host as an empty file.
        let live = |p: &Path| p.symlink_metadata().is_ok();
        for w in self.write.iter().filter(|p| live(p)) {
            a.extend(["--bind".into(), w.clone().into(), w.clone().into()]);
        }
        for g in self.pin.iter().filter(|p| live(p)) {
            a.extend(["--bind".into(), g.clone().into(), g.clone().into()]);
        }
        // After the writable ones, so they win inside them.
        for r in self.ro.iter().filter(|p| live(p)) {
            a.extend(["--ro-bind".into(), r.clone().into(), r.clone().into()]);
        }
        // Last, so they win inside writable folders.
        for (h, dir) in self.hide.iter().filter(|(p, _)| live(p)) {
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

    /// Before a sandboxed command: the git setup to compare against after it
    /// ([`GitSnap::changes`]). `None` when commands aren't sandboxed or outside git.
    pub fn git_snapshot(&self) -> Option<GitSnap> {
        if !self.wraps() {
            return None;
        }
        let probe = self.git.clone()?;
        probe.baseline();
        Some(GitSnap { probe })
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
/// sets env and stdio. Every command gets its own process group (pgid = pid) for
/// group kills, and a new session on Linux, so it can't take the terminal.
/// Example: `zsh -i` made itself the terminal's foreground group, and novacode was stopped
/// ("suspended (tty input)") at its next key read.
pub fn command(sandbox: Option<&Sandbox>, cmd: &str, cwd: &Path) -> io::Result<tokio::process::Command> {
    let mut c = match sandbox {
        Some(s) if s.mode != Mode::Off && s.available() => {
            let l = s.launcher();
            let mut c = tokio::process::Command::new(&l[0]);
            c.args(&l[1..]).args(s.args(cwd)).args(["--", "bash"]);
            c.env_clear().envs(std::env::vars_os().filter(|(k, _)| s.keeps_env(k)));
            novacode_base::platform::own_group(c.as_std_mut());
            c
        }
        Some(s) if s.mode == Mode::On && s.symlink.is_some() => {
            return Err(io::Error::other(format!(
                "the sandbox is on (`sandbox.mode`), but {} is a symlink in a writable place, which the sandbox can't protect, so the command was not run. Replace the symlink with the real file or folder and restart, set `sandbox.mode` to `auto`, or pass `unsandboxed: true` for this one command",
                s.symlink.as_deref().unwrap_or(Path::new("")).display()
            )));
        }
        Some(s) if s.mode == Mode::On && cfg!(windows) => {
            return Err(io::Error::other(
                "the sandbox is on (`sandbox.mode`), but there is no sandbox on Windows, so the command was not run. Set `sandbox.mode` to `auto` or `off`, or pass `unsandboxed: true` for this one command",
            ));
        }
        Some(s) if s.mode == Mode::On => {
            return Err(io::Error::other(
                "the sandbox is on (`sandbox.mode`), but bubblewrap (bwrap) is not installed or doesn't work here, so the command was not run. Install bubblewrap, set `sandbox.mode` to `auto`, or pass `unsandboxed: true` for this one command",
            ));
        }
        // `setsid` (util-linux) doesn't fork when the child isn't a group leader, so the pid stays bash's.
        // pre_exec(setsid) would need `unsafe`, which the workspace forbids.
        #[cfg(target_os = "linux")]
        _ => {
            let mut c = tokio::process::Command::new("setsid");
            c.arg("bash");
            c
        }
        // shortcut: outside Linux an unsandboxed command can still take the terminal; fix when macOS users hit it.
        #[cfg(not(target_os = "linux"))]
        _ => {
            let mut c = tokio::process::Command::new(novacode_base::platform::bash());
            novacode_base::platform::own_group(c.as_std_mut());
            c
        }
    };
    c.arg("-c").arg(cmd).current_dir(cwd);
    Ok(c)
}

/// The rules for one syscall architecture (`seccomp_data.arch`).
struct ArchRules {
    arch: u32,
    /// (syscall number, argument index, value): the call with that argument
    /// (its low 32 bits) equal to the value fails with EPERM.
    rules: &'static [(u32, u32, u32)],
    /// Calls that always fail with EPERM.
    deny: &'static [u32],
    /// x86_64: also refuse x32 syscalls (bit 30 of the number), which share the arch value.
    x32: bool,
}

const AF_UNIX: u32 = 1;
const TIOCSTI: u32 = 0x5412;
/// socketcall's first argument for `socket`: the domain itself is behind a pointer.
const SYS_SOCKET: u32 = 1;
/// io_uring_setup, io_uring_enter, io_uring_register (the same numbers on every
/// architecture): io_uring creates and connects sockets without the filtered calls.
const IO_URING: &[u32] = &[425, 426, 427];

/// x86_64, and 32-bit x86 programs on it (int 0x80).
const X86_64: &[ArchRules] = &[
    ArchRules {
        arch: 0xc000_003e,                            // AUDIT_ARCH_X86_64
        rules: &[(41, 0, AF_UNIX), (16, 1, TIOCSTI)], // socket, ioctl
        deny: IO_URING,
        x32: true,
    },
    ArchRules {
        arch: 0x4000_0003,                                                   // AUDIT_ARCH_I386
        rules: &[(359, 0, AF_UNIX), (102, 0, SYS_SOCKET), (54, 1, TIOCSTI)], // socket, socketcall, ioctl
        deny: IO_URING,
        x32: false,
    },
];

/// aarch64, and 32-bit ARM programs on it.
const AARCH64: &[ArchRules] = &[
    ArchRules {
        arch: 0xc000_00b7,                             // AUDIT_ARCH_AARCH64
        rules: &[(198, 0, AF_UNIX), (29, 1, TIOCSTI)], // socket, ioctl
        deny: IO_URING,
        x32: false,
    },
    ArchRules {
        arch: 0x4000_0028,                                                   // AUDIT_ARCH_ARM
        rules: &[(281, 0, AF_UNIX), (102, 0, SYS_SOCKET), (54, 1, TIOCSTI)], // socket, socketcall, ioctl
        deny: IO_URING,
        x32: false,
    },
];

/// Classic BPF instruction (`struct sock_filter`).
#[derive(Debug, Clone, Copy)]
struct Insn {
    code: u16,
    jt: u8,
    jf: u8,
    k: u32,
}

const LD_ABS: u16 = 0x20; // BPF_LD | BPF_W | BPF_ABS
const JEQ: u16 = 0x15; // BPF_JMP | BPF_JEQ | BPF_K
const JGE: u16 = 0x35; // BPF_JMP | BPF_JGE | BPF_K
const RET: u16 = 0x06; // BPF_RET | BPF_K
const RET_ALLOW: u32 = 0x7fff_0000; // SECCOMP_RET_ALLOW
const RET_EPERM: u32 = 0x0005_0000 | 1; // SECCOMP_RET_ERRNO | EPERM

fn jump(code: u16, k: u32, jt: u8, jf: u8) -> Insn {
    Insn { code, jt, jf, k }
}
fn ld(off: u32) -> Insn {
    jump(LD_ABS, off, 0, 0)
}
fn ret(k: u32) -> Insn {
    jump(RET, k, 0, 0)
}

/// The filter: per architecture, the listed calls fail with EPERM, all else is allowed.
/// `seccomp_data`: `nr` at 0, `arch` at 4, `args[i]` at 16 + 8i (low half first, little-endian).
fn program(arches: &[ArchRules]) -> Vec<Insn> {
    let mut p = vec![ld(4)];
    for a in arches {
        let mut b = vec![ld(0)];
        if a.x32 {
            b.extend([jump(JGE, 0x4000_0000, 0, 1), ret(RET_EPERM)]);
        }
        for &nr in a.deny {
            b.extend([jump(JEQ, nr, 0, 1), ret(RET_EPERM)]);
        }
        for &(nr, arg, value) in a.rules {
            b.extend([
                jump(JEQ, nr, 0, 4),
                ld(16 + 8 * arg),
                jump(JEQ, value, 0, 1),
                ret(RET_EPERM),
                ret(RET_ALLOW),
            ]);
        }
        b.push(ret(RET_ALLOW));
        p.push(jump(JEQ, a.arch, 0, b.len() as u8));
        p.extend(b);
    }
    p.push(ret(RET_ALLOW));
    p
}

/// An anonymous in-memory file for the seccomp program.
#[cfg(target_os = "linux")]
fn memfd() -> io::Result<File> {
    let fd = nix::sys::memfd::memfd_create(c"novacode-seccomp", nix::sys::memfd::MFdFlags::MFD_CLOEXEC)?;
    Ok(File::from(fd))
}

/// memfd is Linux-only, like bubblewrap: elsewhere the sandbox is never available.
#[cfg(not(target_os = "linux"))]
fn memfd() -> io::Result<File> {
    Err(io::Error::other("memfd needs Linux"))
}

/// The filter for this CPU as bytes for `bwrap --seccomp`, or `None` on another architecture.
fn seccomp_program() -> Option<Vec<u8>> {
    let arches: &[ArchRules] = if cfg!(all(target_arch = "x86_64", target_endian = "little")) {
        X86_64
    } else if cfg!(all(target_arch = "aarch64", target_endian = "little")) {
        AARCH64
    } else {
        return None;
    };
    Some(
        program(arches)
            .iter()
            .flat_map(|i| {
                let mut b = Vec::with_capacity(8);
                b.extend(i.code.to_ne_bytes());
                b.extend([i.jt, i.jf]);
                b.extend(i.k.to_ne_bytes());
                b
            })
            .collect(),
    )
}

/// An index file's identity: (inode, size, mtime s, mtime ns, ctime s, ctime ns).
/// git replaces the index by renaming a new file over it, so any write changes
/// it; ctime also catches a rewrite in place with the mtime put back.
type IndexStat = (u64, u64, i64, i64, i64, i64);

#[cfg(unix)]
fn index_stat(p: &Path) -> Option<IndexStat> {
    use std::os::unix::fs::MetadataExt;
    let m = std::fs::metadata(p).ok()?;
    Some((m.ino(), m.size(), m.mtime(), m.mtime_nsec(), m.ctime(), m.ctime_nsec()))
}

// No sandbox on Windows, so no git change check reads this; size and mtime keep it honest.
#[cfg(windows)]
fn index_stat(p: &Path) -> Option<IndexStat> {
    let m = std::fs::metadata(p).ok()?;
    let t = m.modified().ok()?.duration_since(std::time::UNIX_EPOCH).ok()?;
    Some((0, m.len(), t.as_secs() as i64, i64::from(t.subsec_nanos()), 0, 0))
}

/// What the git change check reads (L6.5): `.git` stays writable inside the
/// sandbox, so after each sandboxed command novacode looks whether git's
/// executable setup changed. Nothing is reverted.
struct GitProbe {
    /// The work tree's top: `.gitmodules`, and where `ls-files` runs.
    top: PathBuf,
    /// The git folder first (its `index`), then the shared one of a worktree.
    git_dirs: Vec<PathBuf>,
    /// The gitlinks (mode 160000 entries) of the index at that stat, read
    /// only when the index changed.
    links: Mutex<Option<(Option<IndexStat>, BTreeSet<String>)>>,
    /// The state last reported (or the first one seen), kept for the session:
    /// a change a still-running process makes is reported at the next check.
    last: Mutex<Option<GitState>>,
}

/// The parts of the git setup that can make a later git command run a program.
#[derive(Debug, Clone, PartialEq)]
struct GitState {
    /// Per git folder: its `commondir` (`None` = missing).
    commondir: Vec<Option<Vec<u8>>>,
    gitmodules: Option<Vec<u8>>,
    /// Submodule git folders under `<gitdir>/modules` and their `config`.
    modules: BTreeMap<PathBuf, Option<Vec<u8>>>,
    index: Option<IndexStat>,
    /// The index's gitlinks at `index`.
    links: BTreeSet<String>,
    /// Per gitlink: its work tree's `.git` file, or `.git/config` for an
    /// old-style folder (path relative to the top) and its content.
    link_files: BTreeMap<String, (PathBuf, Option<Vec<u8>>)>,
}

impl GitProbe {
    /// Stats and small reads only; git itself runs only for the index ([`Self::links_at`]).
    fn state(&self) -> GitState {
        let mut modules = BTreeMap::new();
        for g in &self.git_dirs {
            let mut dirs = Vec::new();
            submodule_dirs(&g.join("modules"), 0, &mut dirs);
            for d in dirs {
                let config = std::fs::read(d.join("config")).ok();
                modules.insert(d, config);
            }
        }
        let index = self.git_dirs.first().and_then(|g| index_stat(&g.join("index")));
        let links = self.links_at(index);
        let link_files = links
            .iter()
            .map(|l| {
                let dot = Path::new(l).join(".git");
                let rel = if self.top.join(&dot).is_dir() {
                    dot.join("config")
                } else {
                    dot
                };
                let content = std::fs::read(self.top.join(&rel)).ok();
                (l.clone(), (rel, content))
            })
            .collect();
        GitState {
            commondir: self
                .git_dirs
                .iter()
                .map(|g| std::fs::read(g.join("commondir")).ok())
                .collect(),
            gitmodules: std::fs::read(self.top.join(".gitmodules")).ok(),
            modules,
            index,
            links,
            link_files,
        }
    }

    /// Takes the first state, the one later checks compare against.
    fn baseline(&self) {
        let mut last = self.last.lock().unwrap();
        if last.is_none() {
            *last = Some(self.state());
        }
    }

    /// The changes since the last reported state; that state becomes the new one.
    fn check(&self) -> Option<String> {
        let mut last = self.last.lock().unwrap();
        let new = self.state();
        let line = last.as_ref().and_then(|old| self.diff(old, &new));
        *last = Some(new);
        line
    }

    /// The line for what changed from `old` to `new`, if anything did.
    fn diff(&self, old: &GitState, new: &GitState) -> Option<String> {
        let mut what: Vec<String> = Vec::new();
        let file = |path: String, a: &Option<Vec<u8>>, b: &Option<Vec<u8>>| match (a, b) {
            (None, Some(_)) => Some(format!("{path} created")),
            (Some(_), None) => Some(format!("{path} removed")),
            (Some(x), Some(y)) if x != y => Some(format!("{path} changed")),
            _ => None,
        };
        for (i, g) in self.git_dirs.iter().enumerate() {
            let none = None;
            let (a, b) = (
                old.commondir.get(i).unwrap_or(&none),
                new.commondir.get(i).unwrap_or(&none),
            );
            what.extend(file(self.show(&g.join("commondir")), a, b));
        }
        what.extend(file("`.gitmodules`".into(), &old.gitmodules, &new.gitmodules));
        for (d, config) in &new.modules {
            match old.modules.get(d) {
                None => what.push(format!("new submodule git folder {}", self.show(d))),
                Some(c) if c != config => what.push(format!("{} changed", self.show(&d.join("config")))),
                Some(_) => {}
            }
        }
        for d in old.modules.keys().filter(|d| !new.modules.contains_key(*d)) {
            what.push(format!("submodule git folder {} removed", self.show(d)));
        }
        for l in new.links.difference(&old.links) {
            what.push(format!("new gitlink `{l}` in the index"));
        }
        // A gitlink there before and after: its `.git` file or old-style config.
        for (l, (path, content)) in &new.link_files {
            match old.link_files.get(l) {
                Some((p, c)) if p == path => {
                    what.extend(file(format!("`{}`", path.display()), c, content));
                }
                Some(_) => what.push(format!("`{l}/.git` changed")),
                None => {}
            }
        }
        (!what.is_empty()).then(|| {
            format!(
                "[git setup changed: {}; git commands you run may execute it]",
                what.join(", ")
            )
        })
    }

    /// The gitlinks of the index at `stat`: cached, read again when it changed.
    fn links_at(&self, stat: Option<IndexStat>) -> BTreeSet<String> {
        let mut g = self.links.lock().unwrap();
        if let Some((s, l)) = g.as_ref()
            && *s == stat
        {
            return l.clone();
        }
        let links = if stat.is_some() {
            self.read_links()
        } else {
            BTreeSet::new()
        };
        *g = Some((stat, links.clone()));
        links
    }

    /// `git ls-files --stage` entries with mode 160000.
    fn read_links(&self) -> BTreeSet<String> {
        let out = novacode_base::git::bookkeeping_in(&self.top)
            .args(["ls-files", "--stage", "-z"])
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .output();
        let Ok(out) = out else { return BTreeSet::new() };
        out.stdout
            .split(|b| *b == 0)
            .filter_map(|e| {
                let e = String::from_utf8_lossy(e);
                let (meta, path) = e.split_once('\t')?;
                meta.starts_with("160000 ").then(|| path.to_string())
            })
            .collect()
    }

    /// A path as the user knows it: relative to the work tree when inside it.
    fn show(&self, p: &Path) -> String {
        format!("`{}`", p.strip_prefix(&self.top).unwrap_or(p).display())
    }
}

/// A git change check around a sandboxed command ([`Sandbox::git_snapshot`]).
pub struct GitSnap {
    probe: Arc<GitProbe>,
}

impl GitSnap {
    /// After the command: the line for its result when git's executable setup
    /// changed since the last reported state (a new or changed `commondir`,
    /// `.gitmodules`, a new, removed or changed submodule git folder, a new
    /// gitlink in the index, a changed gitlink `.git` file or old-style
    /// `.git/config`). Also changes another command made meanwhile.
    pub fn changes(&self) -> Option<String> {
        self.probe.check()
    }
}

/// The git folder and the shared git folder of `cwd` (one when they're the same).
pub fn git_dirs(cwd: &Path) -> Vec<PathBuf> {
    let out = novacode_base::git::command_in(cwd)
        .args(["rev-parse", "--path-format=absolute", "--git-dir", "--git-common-dir"])
        .stderr(std::process::Stdio::null())
        .output()
        .ok()
        .filter(|o| o.status.success());
    let mut dirs: Vec<PathBuf> = out
        .map(|o| String::from_utf8_lossy(&o.stdout).lines().map(PathBuf::from).collect())
        .unwrap_or_default();
    dirs.dedup();
    dirs
}

/// The writable project folders of a session: the git top level of `cwd`
/// (falling back to `cwd`), plus the shared git folder of a worktree when it
/// lies outside, so commits there work.
pub fn project_dirs(cwd: &Path) -> Vec<PathBuf> {
    let out = novacode_base::git::command_in(cwd)
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

// bubblewrap, symlinks and Unix modes: nothing here runs on Windows.
#[cfg(all(test, unix))]
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
        for dir in [
            ".ssh",
            ".cache/pip",
            ".cache/zsh",
            ".bun/install/cache",
            ".novacode",
            ".config/gh",
            "extra",
            "proj-secret",
            ".cargo/bin",
            ".cargo/registry",
            ".codex",
            ".mozilla",
            ".local/state/zsh",
            ".var/app",
        ] {
            std::fs::create_dir_all(home.join(dir)).unwrap();
        }
        std::fs::create_dir_all(d.path().join("hist")).unwrap();
        std::fs::write(d.path().join("hist/zsh"), "x").unwrap();
        std::fs::create_dir_all(proj.join("sub")).unwrap();
        std::fs::create_dir_all(proj.join(".git/hooks")).unwrap();
        // A submodule `a/b` with a nested one `c`, and a linked worktree `w`.
        for dir in [
            ".git/modules/a/b/hooks",
            ".git/modules/a/b/modules/c/hooks",
            ".git/modules/a/b/objects/pack",
            ".git/worktrees/w",
        ] {
            std::fs::create_dir_all(proj.join(dir)).unwrap();
        }
        for f in [
            ".novacode/auth.json",
            ".novacode/auth-work.json",
            ".novacode/settings.json",
            ".netrc",
            ".config/gh/hosts.yml",
            ".codex/auth.json",
            ".zsh_history",
            ".python_history",
        ] {
            std::fs::write(home.join(f), "x").unwrap();
        }
        for f in [
            ".env.secret",
            ".git/config",
            ".mcp.json",
            ".git/modules/a/b/HEAD",
            ".git/modules/a/b/config",
            ".git/modules/a/b/modules/c/HEAD",
            ".git/modules/a/b/modules/c/config",
            ".git/worktrees/w/config.worktree",
        ] {
            std::fs::write(proj.join(f), "x").unwrap();
        }
        let c = Config {
            mode: Mode::On,
            project: vec![proj.clone()],
            home: home.clone(),
            novacode_home: home.join(".novacode"),
            tmpdir: None,
            allow_write: vec!["~/extra".into(), "~/missing".into()],
            deny_read: vec![".env.secret".into(), "~/nope".into()],
            credential_dirs: vec![".ssh".into(), ".aws".into(), ".novacode".into()],
            git_dirs: vec![proj.join(".git")],
            secret_env: vec!["My_Provider_Key".into()],
            histfile: Some(d.path().join("hist/zsh")),
        };
        (d, c)
    }

    /// The `--bind`, read-only, hide and `--chdir` entries of an argument list.
    fn entries(a: &[String]) -> Vec<(String, String)> {
        a.iter()
            .enumerate()
            .filter(|(i, s)| {
                ["--bind", "--tmpfs", "--chdir"].contains(&s.as_str())
                    || *s == "/dev/null"
                    || (*s == "--ro-bind" && *i > 0 && a[i + 1] != "/dev/null")
            })
            .map(|(i, s)| (s.clone(), a[i + 1].clone()))
            .collect()
    }

    #[test]
    fn layout_table() {
        let (d, c) = setup();
        let root = std::fs::canonicalize(d.path()).unwrap();
        let r = |p: &str| root.join(p).display().to_string();
        let cwd = root.join("proj/sub");
        let a = strs(&Sandbox::new(c).args(&cwd));
        let pairs = entries(&a);
        let want: Vec<(String, String)> = [
            // Writable, parents first; missing ones (`~/missing`, `~/.npm/_cacache`, …) skipped.
            ("--bind", "/tmp".to_string()),
            ("--bind", r("proj")),
            ("--bind", r("home/extra")),
            // Named download caches only: not `~/.cache/zsh`, `~/.cache` or `~/.bun` as a whole.
            ("--bind", r("home/.cache/pip")),
            // Of ~/.cargo only the download caches; `~/.cargo/git` is missing.
            ("--bind", r("home/.cargo/registry")),
            ("--bind", r("home/.bun/install/cache")),
            // Git folders onto themselves, parents first: a mount point can't be renamed.
            // Every folder down to each submodule git folder, `modules` included.
            ("--bind", r("proj/.git")),
            ("--bind", r("proj/.git/modules")),
            ("--bind", r("proj/.git/modules/a")),
            ("--bind", r("proj/.git/modules/a/b")),
            ("--bind", r("proj/.git/modules/a/b/modules")),
            ("--bind", r("proj/.git/modules/a/b/modules/c")),
            // Read-only inside them: git config and hooks (also of submodules, nested
            // ones first, and worktrees' `config.worktree`), agent config;
            // missing ones (`.git/config.worktree`, `.claude`, `~/.rustup`, …) skipped.
            ("--ro-bind", r("proj/.git/config")),
            ("--ro-bind", r("proj/.git/hooks")),
            ("--ro-bind", r("proj/.git/modules/a/b/modules/c/config")),
            ("--ro-bind", r("proj/.git/modules/a/b/modules/c/hooks")),
            ("--ro-bind", r("proj/.git/modules/a/b/config")),
            ("--ro-bind", r("proj/.git/modules/a/b/hooks")),
            ("--ro-bind", r("proj/.mcp.json")),
            ("--ro-bind", r("proj/.git/worktrees/w/config.worktree")),
            // Hidden: folders get a tmpfs, files /dev/null; `.aws` and `~/nope` don't exist.
            ("--tmpfs", r("home/.ssh")),
            ("/dev/null", r("home/.novacode/auth.json")),
            ("/dev/null", r("home/.novacode/auth-work.json")),
            ("/dev/null", r("home/.netrc")),
            ("/dev/null", r("home/.config/gh/hosts.yml")),
            ("/dev/null", r("home/.codex/auth.json")),
            // Private data: shell and REPL history, Flatpak app data, browser
            // profiles, then `$HISTFILE`; missing ones (`~/.lesshst`, …) skipped.
            ("/dev/null", r("home/.zsh_history")),
            ("--tmpfs", r("home/.local/state/zsh")),
            ("/dev/null", r("home/.python_history")),
            ("--tmpfs", r("home/.var/app")),
            ("--tmpfs", r("home/.mozilla")),
            ("/dev/null", r("hist/zsh")),
            ("/dev/null", r("proj/.env.secret")),
            ("--chdir", cwd.display().to_string()),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect();
        assert_eq!(pairs, want, "{a:?}");
        assert_eq!(&a[..5], ["--ro-bind", "/", "/", "--dev", "/dev"]);
        assert!(a.contains(&"--unshare-pid".into()) && a.contains(&"--die-with-parent".into()));
        assert!(a.contains(&"--new-session".into()), "no terminal inside");
        // The whole ~/.novacode stays readable (settings, skills).
        assert!(!a.contains(&r("home/.novacode")));
    }

    #[test]
    fn vanished_paths_are_skipped() {
        let (d, c) = setup();
        let root = std::fs::canonicalize(d.path()).unwrap();
        let s = Sandbox::new(c);
        // Deleted mid-session: a writable folder, a read-only one, a hidden file and folder.
        std::fs::remove_dir(root.join("home/extra")).unwrap();
        std::fs::remove_dir(root.join("proj/.git/hooks")).unwrap();
        std::fs::remove_file(root.join("home/.netrc")).unwrap();
        std::fs::remove_dir(root.join("home/.ssh")).unwrap();
        let a = strs(&s.args(&root.join("proj")));
        for gone in ["home/extra", "proj/.git/hooks", "home/.netrc", "home/.ssh"] {
            let p = root.join(gone).display().to_string();
            assert!(!a.contains(&p), "{gone} still bound: {a:?}");
        }
        assert!(a.contains(&root.join("home/.config/gh/hosts.yml").display().to_string()));
    }

    #[test]
    fn secret_env_table() {
        let (_d, c) = setup();
        let s = Sandbox::new(c);
        for (name, kept) in [
            ("PATH", true),
            ("HOME", true),
            ("LANG", true),
            ("GEMINI_API_KEY", false),
            ("DIGITALOCEAN_API_TOKEN", false),
            ("github_token", false),
            ("AWS_SECRET_ACCESS_KEY", false),
            ("AWS_ACCESS_KEY_ID", false),
            ("PGPASSWORD", false),
            ("FTP_PASSWD", false),
            ("STRIPE_APIKEY", false),
            ("SSH_PRIVATE_KEY", false),
            ("GOOGLE_APPLICATION_CREDENTIALS", false),
            ("SSH_AUTH_SOCK", false),
            ("GPG_AGENT_INFO", false),
            ("DBUS_SESSION_BUS_ADDRESS", false),
            ("DOCKER_HOST", false),
            // A provider's key variable from the profiles, matched case-insensitively.
            ("MY_PROVIDER_KEY", false),
            ("MY_PROVIDER", true),
            // Names ending in `_KEY`/`_KEYS`, and `AUTH` as a word.
            ("DEPLOY_KEY", false),
            ("signing_keys", false),
            ("MY_AUTH", false),
            ("AUTH_TOKEN", false),
            ("GH_AUTH", false),
            ("AUTH", false),
            ("GIT_AUTHOR_NAME", true),
            ("GIT_AUTHOR_EMAIL", true),
            ("OAUTHLIB_DEBUG", true),
            ("KEYBOARD", true),
            ("KEYS_DIR", true),
        ] {
            assert_eq!(s.keeps_env(OsStr::new(name)), kept, "{name}");
        }
    }

    /// Runs a filter on one syscall: (nr, arch, args).
    fn bpf(p: &[Insn], nr: u32, arch: u32, args: [u64; 6]) -> u32 {
        let load = |off: u32| match off {
            0 => nr,
            4 => arch,
            o => {
                let (i, hi) = (((o - 16) / 8) as usize, (o - 16) % 8 == 4);
                (if hi { args[i] >> 32 } else { args[i] }) as u32
            }
        };
        let (mut pc, mut acc) = (0usize, 0u32);
        loop {
            let i = p[pc];
            pc += 1;
            match i.code {
                LD_ABS => acc = load(i.k),
                RET => return i.k,
                JEQ | JGE => {
                    let hit = if i.code == JEQ { acc == i.k } else { acc >= i.k };
                    pc += usize::from(if hit { i.jt } else { i.jf });
                }
                c => panic!("unknown op {c:#x}"),
            }
        }
    }

    #[test]
    fn seccomp_filter_table() {
        let (x86, i386, arm64, arm) = (0xc000_003e, 0x4000_0003, 0xc000_00b7, 0x4000_0028);
        let (x, a) = (program(X86_64), program(AARCH64));
        let unix = [1, 1, 0, 0, 0, 0];
        let inet = [2, 1, 0, 0, 0, 0];
        let tiocsti = [0, 0x5412, 0, 0, 0, 0];
        for (p, nr, arch, args, want, what) in [
            (&x, 41, x86, unix, RET_EPERM, "socket(AF_UNIX)"),
            // The kernel takes the domain as int: high bits don't hide AF_UNIX.
            (
                &x,
                41,
                x86,
                [(7 << 32) | 1, 1, 0, 0, 0, 0],
                RET_EPERM,
                "socket(AF_UNIX | high bits)",
            ),
            (&x, 41, x86, inet, RET_ALLOW, "socket(AF_INET)"),
            (&x, 53, x86, unix, RET_ALLOW, "socketpair(AF_UNIX)"),
            (&x, 16, x86, tiocsti, RET_EPERM, "ioctl(TIOCSTI)"),
            (&x, 16, x86, [0, 0x5401, 0, 0, 0, 0], RET_ALLOW, "ioctl(TCGETS)"),
            (&x, 0x4000_0029, x86, unix, RET_EPERM, "x32 socket"),
            (&x, 0, x86, unix, RET_ALLOW, "read"),
            (&x, 359, i386, unix, RET_EPERM, "i386 socket(AF_UNIX)"),
            (
                &x,
                102,
                i386,
                [1, 0, 0, 0, 0, 0],
                RET_EPERM,
                "i386 socketcall(SYS_SOCKET)",
            ),
            (
                &x,
                102,
                i386,
                [8, 0, 0, 0, 0, 0],
                RET_ALLOW,
                "i386 socketcall(SYS_SOCKETPAIR)",
            ),
            (&x, 54, i386, tiocsti, RET_EPERM, "i386 ioctl(TIOCSTI)"),
            (&x, 41, 0x1234, unix, RET_ALLOW, "unknown arch"),
            // io_uring makes sockets without socket(): refused whatever its arguments.
            (&x, 425, x86, inet, RET_EPERM, "io_uring_setup"),
            (&x, 426, x86, inet, RET_EPERM, "io_uring_enter"),
            (&x, 427, x86, inet, RET_EPERM, "io_uring_register"),
            (&x, 424, x86, inet, RET_ALLOW, "pidfd_send_signal"),
            (&x, 425, i386, inet, RET_EPERM, "i386 io_uring_setup"),
            (&x, 427, i386, inet, RET_EPERM, "i386 io_uring_register"),
            (&a, 425, arm64, inet, RET_EPERM, "arm64 io_uring_setup"),
            (&a, 426, arm64, inet, RET_EPERM, "arm64 io_uring_enter"),
            (&a, 427, arm, inet, RET_EPERM, "arm io_uring_register"),
            (&a, 428, arm64, inet, RET_ALLOW, "arm64 open_tree"),
            (&a, 198, arm64, unix, RET_EPERM, "arm64 socket(AF_UNIX)"),
            (&a, 198, arm64, inet, RET_ALLOW, "arm64 socket(AF_INET)"),
            (&a, 199, arm64, unix, RET_ALLOW, "arm64 socketpair"),
            (&a, 29, arm64, tiocsti, RET_EPERM, "arm64 ioctl(TIOCSTI)"),
            (&a, 0x4000_0029, arm64, unix, RET_ALLOW, "arm64 has no x32"),
            (&a, 281, arm, unix, RET_EPERM, "arm socket(AF_UNIX)"),
            (&a, 54, arm, tiocsti, RET_EPERM, "arm ioctl(TIOCSTI)"),
            // x86_64's socket number on arm64 is another call.
            (&a, 41, arm64, unix, RET_ALLOW, "arm64 nr 41"),
        ] {
            assert_eq!(bpf(p, nr, arch, args), want, "{what}");
        }
        // bwrap reads whole `struct sock_filter`s.
        if let Some(b) = seccomp_program() {
            assert_eq!(b.len() % 8, 0);
        }
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
            assert!(!cmd.as_std().get_args().any(|a| a == "--"), "{mode:?} runs unsandboxed");
        }
        assert_eq!(Mode::parse("on"), Some(Mode::On));
        assert_eq!(Mode::parse("yes"), None);
        assert!(
            hint_for(Some(&s), true, "Read-only file system").is_none(),
            "not sandboxed"
        );
    }

    /// A worktree: its `.git` file is read-only, its git folder and the shared
    /// one (writable, outside the worktree) are mount points.
    #[test]
    fn worktree_layout() {
        let d = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(d.path()).unwrap();
        let (wt, common) = (root.join("wt"), root.join("main/.git"));
        let gd = common.join("worktrees/wt");
        std::fs::create_dir_all(&wt).unwrap();
        std::fs::create_dir_all(common.join("hooks")).unwrap();
        std::fs::create_dir_all(&gd).unwrap();
        std::fs::write(wt.join(".git"), format!("gitdir: {}\n", gd.display())).unwrap();
        std::fs::write(common.join("config"), "").unwrap();
        let s = Sandbox::new(Config {
            project: vec![wt.clone(), common.clone()],
            home: root.join("home"),
            novacode_home: root.join("home/.novacode"),
            git_dirs: vec![gd.clone(), common.clone()],
            ..Config::default()
        });
        assert!(s.symlink().is_none());
        let a = strs(&s.args(&wt));
        let pairs = entries(&a);
        let s = |p: &Path| p.display().to_string();
        for want in [
            ("--bind", s(&common)),
            ("--bind", s(&gd)),
            ("--ro-bind", s(&common.join("config"))),
            ("--ro-bind", s(&common.join("hooks"))),
            ("--ro-bind", s(&wt.join(".git"))),
        ] {
            assert!(pairs.contains(&(want.0.to_string(), want.1.clone())), "{want:?}: {a:?}");
        }
        // The shared folder is bound once, as a writable place.
        assert_eq!(a.iter().filter(|x| **x == s(&common)).count(), 2, "{a:?}");
    }

    /// A protected path that is a symlink in a writable place: no sandbox.
    #[test]
    fn protected_symlinks_make_it_unavailable() {
        // Untouched layout: fine.
        assert!(Sandbox::new(setup().1).symlink().is_none());
        type Make = fn(&Path);
        let cases: [(&str, Make); 3] = [
            ("proj/.git/hooks", |r| {
                std::fs::remove_dir(r.join("proj/.git/hooks")).unwrap();
                std::os::unix::fs::symlink("../realhooks", r.join("proj/.git/hooks")).unwrap();
            }),
            // Dangling: the target can be created inside.
            ("proj/.claude", |r| {
                std::os::unix::fs::symlink("elsewhere", r.join("proj/.claude")).unwrap();
            }),
            // A hidden file under a linked folder.
            ("proj/linked", |r| {
                std::fs::create_dir_all(r.join("home/extra/real")).unwrap();
                std::fs::write(r.join("home/extra/real/.env.secret"), "x").unwrap();
                std::os::unix::fs::symlink(r.join("home/extra/real"), r.join("proj/linked")).unwrap();
            }),
        ];
        for (path, make) in cases {
            let (d, mut c) = setup();
            let root = std::fs::canonicalize(d.path()).unwrap();
            make(&root);
            let cwd = c.project[0].clone();
            c.deny_read.push("linked/.env.secret".into());
            let s = Sandbox::new(c.clone());
            assert_eq!(s.symlink(), Some(root.join(path).as_path()), "{path}");
            assert!(!s.available() && !s.wraps(), "{path}");
            let e = command(Some(&s), "true", &cwd).unwrap_err().to_string();
            assert!(
                e.contains(&root.join(path).display().to_string()) && e.contains("symlink"),
                "{e}"
            );
            let s = Sandbox::new(Config { mode: Mode::Auto, ..c });
            let cmd = command(Some(&s), "true", &cwd).unwrap();
            assert!(
                !cmd.as_std().get_args().any(|a| a == "--"),
                "auto runs {path} unsandboxed"
            );
        }
        // A symlink outside every writable place can't be swapped: fine.
        // Under the real home, not /tmp: /tmp is writable.
        let d = tempfile::Builder::new()
            .prefix(".novacode-sandbox-test")
            .tempdir_in(std::env::var_os("HOME").unwrap())
            .unwrap();
        let home = d.path().join("home");
        std::fs::create_dir_all(home.join("real")).unwrap();
        std::fs::create_dir_all(d.path().join("proj")).unwrap();
        std::fs::write(home.join("real/secret"), "x").unwrap();
        std::os::unix::fs::symlink(home.join("real"), home.join("cfg")).unwrap();
        let s = Sandbox::new(Config {
            project: vec![d.path().join("proj")],
            home: home.clone(),
            novacode_home: home.join(".novacode"),
            deny_read: vec!["~/cfg/secret".into()],
            ..Config::default()
        });
        assert!(s.symlink().is_none());
        assert!(strs(&s.args(d.path())).contains(&home.join("real/secret").display().to_string()));
    }

    /// Inherited descriptors are closed, so commands don't get them.
    #[cfg(unix)]
    #[test]
    fn inherited_fds_are_closed() {
        use std::os::fd::IntoRawFd;
        // `dup` leaves the copy without close-on-exec, like a descriptor from a parent shell.
        let fd = nix::unistd::dup(std::io::stderr()).unwrap().into_raw_fd();
        assert!(open_fds().contains(&fd));
        let seen = || {
            std::process::Command::new("sh")
                .args(["-c", &format!("test -e /proc/self/fd/{fd}")])
                .status()
                .unwrap()
                .success()
        };
        assert!(seen(), "fd {fd} should reach a child before");
        close_fds(&[fd]);
        assert!(!seen(), "fd {fd} still reaches a child");
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

    /// A temp folder under the real home (not /tmp: /tmp is writable inside).
    fn home_tempdir() -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix(".novacode-sandbox-test")
            .tempdir_in(std::env::var_os("HOME").unwrap())
            .unwrap()
    }

    /// Commands can't open the terminal (`--new-session`). The test makes its
    /// own terminal with `script`, so it runs without one too.
    #[tokio::test]
    async fn real_no_terminal() {
        let d = home_tempdir();
        let proj = std::fs::canonicalize(d.path()).unwrap();
        let sb = Sandbox::new(Config {
            mode: Mode::On,
            project: vec![proj.clone()],
            home: proj.join("home"),
            novacode_home: proj.join("home/.novacode"),
            ..Config::default()
        });
        if !sb.available() || !have("script") {
            eprintln!("note: real_no_terminal skipped, bwrap or script is not available here");
            return;
        }
        let q = |s: &OsStr| format!("'{}'", s.to_string_lossy().replace('\'', r"'\''"));
        let in_tty = |line: String| {
            let o = std::process::Command::new("script")
                .args(["-qec", &line, "/dev/null"])
                .stdin(std::process::Stdio::null())
                .output()
                .unwrap();
            String::from_utf8_lossy(&o.stdout).into_owned()
        };
        let probe = "exec 3<>/dev/tty && echo opened";
        if !in_tty(format!("bash -c {}", q(OsStr::new(probe)))).contains("opened") {
            eprintln!("note: real_no_terminal skipped, script gives no terminal here");
            return;
        }
        for sb in [Some(&sb), None] {
            let c = command(sb, probe, &proj).unwrap();
            let c = c.as_std();
            let line: Vec<String> = std::iter::once(c.get_program()).chain(c.get_args()).map(q).collect();
            // `; true` keeps the terminal's session leader alive, as novacode's shell is.
            let t = in_tty(line.join(" ") + "; true");
            assert!(
                !t.contains("opened"),
                "/dev/tty opened (sandboxed: {}): {t}",
                sb.is_some()
            );
        }
    }

    /// Real runs: submodule git folders can't be renamed, and the git change
    /// check reports the reviewer's attacks but not a normal commit.
    #[tokio::test]
    async fn real_git_pins_and_change_check() {
        use crate::Tool;
        use crate::testutil::{ctx, text};
        let d = home_tempdir();
        let root = std::fs::canonicalize(d.path()).unwrap();
        let proj = root.join("proj");
        std::fs::create_dir_all(&proj).unwrap();
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .args([
                    "-c",
                    "user.name=t",
                    "-c",
                    "user.email=t@t",
                    "-c",
                    "commit.gpgsign=false",
                ])
                .args(args)
                .current_dir(&proj)
                .output()
                .is_ok_and(|o| o.status.success())
        };
        if !git(&["init", "-q"]) || !git(&["commit", "-q", "--allow-empty", "-m", "x"]) {
            eprintln!("note: real_git_pins_and_change_check skipped, git doesn't work here");
            return;
        }
        // A submodule git folder, there at the start.
        let sm = proj.join(".git/modules/sm");
        std::fs::create_dir_all(&sm).unwrap();
        std::fs::write(sm.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        std::fs::write(sm.join("config"), "[core]\n").unwrap();
        let sb = Arc::new(Sandbox::new(Config {
            mode: Mode::On,
            project: project_dirs(&proj),
            home: root.join("home"),
            novacode_home: root.join("home/.novacode"),
            git_dirs: git_dirs(&proj),
            ..Config::default()
        }));
        if !sb.available() {
            eprintln!("note: real_git_pins_and_change_check skipped, bwrap is not available here");
            return;
        }
        let s = Some(sb.as_ref());
        // Pinned: neither the submodule git folder nor `modules` can be renamed.
        for cmd in ["mv .git/modules/sm .git/modules/old", "mv .git/modules .git/m2"] {
            let (ok, t) = sh(s, &proj, cmd).await;
            assert!(!ok && t.contains("busy"), "{cmd}: {t}");
        }
        assert!(sm.join("HEAD").exists());
        let changed = |cmd: &'static str| {
            let sb = sb.clone();
            let proj = proj.clone();
            async move {
                let snap = sb.git_snapshot().unwrap();
                let (ok, t) = sh(Some(sb.as_ref()), &proj, cmd).await;
                assert!(ok, "{cmd}: {t}");
                snap.changes()
            }
        };
        // A planted `commondir`, through the bash tool: the line ends the result.
        let mut c = ctx(&proj);
        c.env = Arc::new(crate::SandboxedEnv(sb.clone()));
        let o = crate::bash::Bash
            .execute(serde_json::json!({"command": "echo /tmp/evil > .git/commondir"}), &c)
            .await;
        assert_eq!(
            text(&o),
            "(no output)\n[git setup changed: `.git/commondir` created; git commands you run may execute it]"
        );
        // Through the process manager: bash's result, and `process` start's notice when it exits.
        let pm = crate::process::ProcessManager::new(&root.join("procs"), "s");
        c.processes = Some(pm.clone());
        let o = crate::bash::Bash
            .execute(serde_json::json!({"command": "echo /tmp/other > .git/commondir"}), &c)
            .await;
        assert!(
            text(&o).ends_with("[git setup changed: `.git/commondir` changed; git commands you run may execute it]"),
            "{}",
            text(&o)
        );
        let mut events = pm.events.subscribe();
        pm.start("rm", "rm .git/commondir", &proj, &[], true, s).unwrap();
        let notice = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                if let Ok(crate::process::Event::Notice { text, .. }) = events.recv().await
                    && text.contains("git setup changed")
                {
                    return text;
                }
            }
        })
        .await
        .unwrap();
        assert!(notice.contains("`.git/commondir` removed"), "{notice}");
        // A submodule added mid-session (not pinned), renamed and replaced by one with `core.fsmonitor`.
        let late = proj.join(".git/modules/late");
        std::fs::create_dir_all(&late).unwrap();
        std::fs::write(late.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        std::fs::write(late.join("config"), "[core]\n").unwrap();
        // Made outside a check: the next one reports it (against the last reported state).
        let t = changed("true").await.unwrap_or_default();
        assert!(t.contains("new submodule git folder `.git/modules/late`"), "{t}");
        let t = changed(
            r#"mv .git/modules/late .git/modules/old && mkdir .git/modules/late && cp .git/modules/old/HEAD .git/modules/late/ && printf '[core]\n\tfsmonitor = touch PWNED\n' > .git/modules/late/config"#,
        )
        .await
        .unwrap_or_default();
        assert!(
            t.contains("new submodule git folder `.git/modules/old`")
                && t.contains("`.git/modules/late/config` changed"),
            "{t}"
        );
        // An embedded repo with `core.fsmonitor`, added as a gitlink.
        let t = changed(
            "mkdir nested && cd nested && git init -q && git -c user.name=t -c user.email=t@t -c commit.gpgsign=false commit -q --allow-empty -m x && git config core.fsmonitor 'touch PWNED' && cd .. && git add nested 2>/dev/null",
        )
        .await
        .unwrap_or_default();
        assert!(t.contains("new gitlink `nested` in the index"), "{t}");
        // A normal commit is not reported.
        let t = changed(
            "echo y > f.txt && git add f.txt && git -c user.name=t -c user.email=t@t -c commit.gpgsign=false commit -qm y",
        )
        .await;
        assert_eq!(t, None);
        assert!(!proj.join("PWNED").exists());
        // A still-running background process plants `commondir` after its start
        // and after a bash call's check: a later bash call reports it, its exit not again.
        let mut events = pm.events.subscribe();
        let id = pm
            .start(
                "late",
                "sleep 1; echo /tmp/evil > .git/commondir; sleep 60",
                &proj,
                &[],
                true,
                s,
            )
            .unwrap();
        assert_eq!(changed("true").await, None);
        for _ in 0..100 {
            if proj.join(".git/commondir").exists() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        let t = changed("true").await.unwrap_or_default();
        assert!(t.contains("`.git/commondir` created"), "{t}");
        pm.stop(&id).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(800)).await;
        while let Ok(e) = events.try_recv() {
            if let crate::process::Event::Notice { text, .. } = e {
                assert!(!text.contains("git setup changed"), "reported again: {text}");
            }
        }
    }

    /// The git change check without bwrap: a rewritten gitlink `.git` file and
    /// old-style `.git/config`, and a same-size index rewrite with the mtime put back.
    #[test]
    fn git_check_links_and_index() {
        let d = tempfile::tempdir().unwrap();
        let proj = std::fs::canonicalize(d.path()).unwrap();
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .args([
                    "-c",
                    "user.name=t",
                    "-c",
                    "user.email=t@t",
                    "-c",
                    "commit.gpgsign=false",
                ])
                .args(args)
                .current_dir(&proj)
                .output()
                .ok()
                .filter(|o| o.status.success())
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        };
        if git(&["init", "-q"]).is_none() || git(&["commit", "-q", "--allow-empty", "-m", "x"]).is_none() {
            eprintln!("note: git_check_links_and_index skipped, git doesn't work here");
            return;
        }
        let head = git(&["rev-parse", "HEAD"]).unwrap();
        for l in ["sm", "old"] {
            git(&["update-index", "--add", "--cacheinfo", &format!("160000,{head},{l}")]).unwrap();
        }
        std::fs::create_dir_all(proj.join("sm")).unwrap();
        std::fs::write(proj.join("sm/.git"), "gitdir: ../.git/modules/sm\n").unwrap();
        std::fs::create_dir_all(proj.join("old/.git")).unwrap();
        std::fs::write(proj.join("old/.git/config"), "[core]\n").unwrap();
        let probe = GitProbe {
            top: proj.clone(),
            git_dirs: git_dirs(&proj),
            links: Mutex::new(None),
            last: Mutex::new(None),
        };
        probe.baseline();
        assert_eq!(probe.check(), None);
        std::fs::write(proj.join("sm/.git"), "gitdir: /tmp/evil\n").unwrap();
        std::fs::write(proj.join("old/.git/config"), "[core]\n\tfsmonitor = x\n").unwrap();
        let t = probe.check().unwrap_or_default();
        assert!(
            t.contains("`sm/.git` changed") && t.contains("`old/.git/config` changed"),
            "{t}"
        );
        assert_eq!(probe.check(), None, "reported once");
        // Two indexes of the same size: `zz` a plain file, then a gitlink.
        let index = proj.join(".git/index");
        git(&["update-index", "--add", "--cacheinfo", &format!("100644,{head},zz")]).unwrap();
        let plain = std::fs::read(&index).unwrap();
        git(&["update-index", "--cacheinfo", &format!("160000,{head},zz")]).unwrap();
        let link = std::fs::read(&index).unwrap();
        assert_eq!(plain.len(), link.len());
        std::fs::write(&index, &plain).unwrap();
        assert_eq!(probe.check(), None);
        let before = index_stat(&index).unwrap();
        let mtime = std::fs::metadata(&index).unwrap().modified().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        // Rewritten in place, mtime put back: only ctime tells.
        std::fs::write(&index, &link).unwrap();
        std::fs::File::options()
            .write(true)
            .open(&index)
            .unwrap()
            .set_modified(mtime)
            .unwrap();
        let after = index_stat(&index).unwrap();
        assert_eq!(
            (before.0, before.1, before.2, before.3),
            (after.0, after.1, after.2, after.3),
            "inode, size and mtime are the same"
        );
        let t = probe.check().unwrap_or_default();
        assert!(t.contains("new gitlink `zz` in the index"), "{t}");
    }

    #[test]
    fn project_dirs_fall_back_to_cwd() {
        let d = tempfile::tempdir().unwrap();
        assert_eq!(project_dirs(d.path()), vec![d.path().to_path_buf()]);
        assert!(git_dirs(d.path()).is_empty());
    }

    /// Runs `cmd` through [`command`]: (success, stdout + stderr).
    async fn sh(sb: Option<&Sandbox>, cwd: &Path, cmd: &str) -> (bool, String) {
        let o = command(sb, cmd, cwd).unwrap().output().await.unwrap();
        let mut t = String::from_utf8_lossy(&o.stdout).into_owned();
        t.push_str(&String::from_utf8_lossy(&o.stderr));
        (o.status.success(), t.trim().to_string())
    }

    fn have(bin: &str) -> bool {
        std::process::Command::new("sh")
            .args(["-c", &format!("command -v {bin}")])
            .stdout(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    }

    /// Real runs: unix sockets, the environment, read-only git state, vanished paths.
    #[tokio::test]
    async fn real_filter_env_and_read_only() {
        let base = std::env::var_os("HOME").map(PathBuf::from).unwrap();
        let d = tempfile::Builder::new()
            .prefix(".novacode-sandbox-test")
            .tempdir_in(&base)
            .unwrap();
        let root = std::fs::canonicalize(d.path()).unwrap();
        let (home, proj) = (root.join("home"), root.join("proj"));
        std::fs::create_dir_all(home.join(".cargo/registry")).unwrap();
        std::fs::create_dir_all(proj.join(".git/hooks")).unwrap();
        std::fs::write(proj.join(".git/config"), "[core]\n").unwrap();
        std::fs::write(proj.join("secret.txt"), "S").unwrap();
        let sb = Sandbox::new(Config {
            mode: Mode::On,
            project: vec![proj.clone()],
            home: home.clone(),
            novacode_home: home.join(".novacode"),
            deny_read: vec!["secret.txt".into()],
            git_dirs: vec![proj.join(".git")],
            // HOME stands in for a provider key: the test can't set variables.
            secret_env: vec!["HOME".into()],
            ..Config::default()
        });
        if !sb.available() {
            eprintln!("note: real_filter_env_and_read_only skipped, bwrap is not available here");
            return;
        }
        let s = Some(&sb);
        // Secrets are dropped, the rest stays; `unsandboxed` (no sandbox) keeps everything.
        let probe = r#"echo "${HOME-unset} ${PATH:+path}""#;
        assert_eq!(sh(s, &proj, probe).await, (true, "unset path".into()));
        assert_ne!(sh(None, &proj, probe).await.1, "unset path");
        // Git config and hooks are read-only; the rest of .git stays writable.
        let (ok, t) = sh(s, &proj, "echo x >> .git/config").await;
        assert!(!ok && t.contains("Read-only file system"), "{t}");
        let (ok, t) = sh(s, &proj, "touch .git/hooks/pre-commit").await;
        assert!(!ok && t.contains("Read-only file system"), "{t}");
        assert!(sh(s, &proj, "touch .git/index.lock").await.0);
        // The git folder can't be swapped for a copy with a planted `core.fsmonitor`.
        let (ok, t) = sh(
            s,
            &proj,
            r#"mv .git .git-old && cp -a .git-old .git && printf '[core]\n\tfsmonitor = touch PWNED\n' > .git/config"#,
        )
        .await;
        assert!(!ok && t.contains("busy"), "{t}");
        assert!(!proj.join(".git-old").exists());
        assert!(!sh(s, &proj, "rm -rf .git").await.0);
        assert_eq!(std::fs::read_to_string(proj.join(".git/config")).unwrap(), "[core]\n");
        // ~/.cargo: config can't be created, the download cache is writable.
        let cargo = home.join(".cargo");
        let (ok, t) = sh(s, &proj, &format!("touch {}/config.toml", cargo.display())).await;
        assert!(!ok && t.contains("Read-only file system"), "{t}");
        assert!(sh(s, &proj, &format!("touch {}/registry/x", cargo.display())).await.0);
        // A hidden file deleted mid-session: commands still run, and it is not recreated.
        std::fs::remove_file(proj.join("secret.txt")).unwrap();
        assert_eq!(sh(s, &proj, "echo ok").await, (true, "ok".into()));
        assert!(!proj.join("secret.txt").exists());
        // Unix sockets.
        if seccomp_program().is_none() {
            eprintln!("note: no seccomp filter for this CPU, unix socket checks skipped");
            return;
        }
        if have("python3") {
            let (ok, t) = sh(s, &proj, r#"python3 -c "import socket; socket.socket(socket.AF_UNIX)""#).await;
            assert!(!ok && t.contains("Operation not permitted"), "{t}");
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let port = l.local_addr().unwrap().port();
            let tcp = format!(
                r#"python3 -c "import socket; socket.create_connection(('127.0.0.1', {port})); socket.socketpair(); print('ok')""#
            );
            assert_eq!(sh(s, &proj, &tcp).await, (true, "ok".into()));
            // io_uring_setup(1, NULL): EFAULT or ENOSYS outside, EPERM (1) inside.
            let uring = r#"python3 -c "import ctypes; l = ctypes.CDLL(None, use_errno=True); print(l.syscall(425, 1, None), ctypes.get_errno())""#;
            match sh(s, &proj, uring).await {
                (true, t) => assert_eq!(t, "-1 1", "io_uring_setup inside"),
                (false, t) => eprintln!("note: python3 without ctypes, io_uring check skipped: {t}"),
            }
        } else {
            eprintln!("note: python3 missing, socket checks skipped");
        }
        // Desktop and Docker sockets reachable outside are refused inside.
        for (bin, cmd) in [("busctl", "busctl --user list"), ("docker", "docker version")] {
            if !have(bin) || !sh(None, &proj, cmd).await.0 {
                eprintln!("note: `{cmd}` doesn't work outside, its check is skipped");
                continue;
            }
            let (ok, t) = sh(s, &proj, cmd).await;
            assert!(!ok, "{cmd} reached its socket inside: {t}");
        }
    }
}
