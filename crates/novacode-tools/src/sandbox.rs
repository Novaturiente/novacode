//! Sandbox (L6.5): commands the model runs (`bash`, `process` start) run under
//! bubblewrap. The whole file system is read-only; the project, `/tmp` and the
//! tool caches are writable; credentials are hidden; the network stays on.
//! Executable state inside writable places (git config and hooks, tool binaries)
//! stays read-only, unix sockets are blocked by a seccomp filter, and secrets
//! are dropped from the environment.
//! [`command`] builds every model command, so the bash and process paths can't drift.

use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io::{self, Write};
use std::os::fd::AsRawFd;
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
    /// The git folder and the shared git folder of the project ([`git_dirs`]):
    /// their `config` and `hooks` stay read-only.
    pub git_dirs: Vec<PathBuf>,
    /// Environment variables holding provider keys, dropped from sandboxed commands.
    pub secret_env: Vec<String>,
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
const RO_HOME: &[&str] = &[
    ".cargo/bin",
    ".cargo/config",
    ".cargo/config.toml",
    ".cargo/env",
    ".rustup",
    ".bun/bin",
    "go/bin",
];
/// Project files novacode and other agents run or load, read-only when they exist.
const RO_PROJECT: &[&str] = &[".novacode", ".claude", ".mcp.json"];
/// Files in a git folder that git runs commands from (`core.fsmonitor`, hooks).
const RO_GIT: &[&str] = &["config", "config.worktree", "hooks"];
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
        let mut ro: Vec<PathBuf> = Vec::new();
        let ro_cands = c
            .git_dirs
            .iter()
            .flat_map(|g| RO_GIT.iter().map(move |f| g.join(f)))
            .chain(RO_PROJECT.iter().map(|f| base.join(f)))
            .chain(RO_HOME.iter().map(|f| c.home.join(f)));
        for p in ro_cands {
            if let Ok(real) = std::fs::canonicalize(&p)
                && !ro.contains(&real)
            {
                ro.push(real);
            }
        }
        Sandbox {
            mode: c.mode,
            write,
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
    /// or `None` without a filter for this CPU.
    fn filter_path(&self) -> Option<String> {
        let memfd = self.filter.get_or_init(|| {
            let made = seccomp_program().map(|bytes| -> io::Result<File> {
                let fd = nix::sys::memfd::memfd_create(c"novacode-seccomp", nix::sys::memfd::MFdFlags::MFD_CLOEXEC)?;
                let mut f = File::from(fd);
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
        memfd
            .as_ref()
            .map(|m| format!("/proc/{}/fd/{}", std::process::id(), m.as_raw_fd()))
    }

    /// The variable is passed into sandboxed commands (not a secret).
    fn keeps_env(&self, name: &OsStr) -> bool {
        let n = name.to_string_lossy().to_ascii_uppercase();
        !(SECRET_NAMES.contains(&n.as_str())
            || SECRET_PARTS.iter().any(|p| n.contains(p))
            || self.secret_env.iter().any(|s| s.eq_ignore_ascii_case(&n)))
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    /// bwrap works here: checked once, then cached. In `auto` mode a failed
    /// check logs the one warning.
    pub fn available(&self) -> bool {
        *self.works.get_or_init(|| {
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
            // No `--new-session`: the command stays in the caller's process group for killpg.
            "--unshare-pid",
            "--die-with-parent",
        ]
        .map(OsString::from)
        .into();
        // A path deleted since the start is skipped: bwrap would fail on it, or
        // create it on the host as an empty file.
        let live = |p: &Path| p.symlink_metadata().is_ok();
        for w in self.write.iter().filter(|p| live(p)) {
            a.extend(["--bind".into(), w.clone().into(), w.clone().into()]);
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
            let l = s.launcher();
            let mut c = tokio::process::Command::new(&l[0]);
            c.args(&l[1..]).args(s.args(cwd)).args(["--", "bash"]);
            c.env_clear().envs(std::env::vars_os().filter(|(k, _)| s.keeps_env(k)));
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

/// The rules for one syscall architecture (`seccomp_data.arch`).
struct ArchRules {
    arch: u32,
    /// (syscall number, argument index, value): the call with that argument
    /// (its low 32 bits) equal to the value fails with EPERM.
    rules: &'static [(u32, u32, u32)],
    /// x86_64: also refuse x32 syscalls (bit 30 of the number), which share the arch value.
    x32: bool,
}

const AF_UNIX: u32 = 1;
const TIOCSTI: u32 = 0x5412;
/// socketcall's first argument for `socket`: the domain itself is behind a pointer.
const SYS_SOCKET: u32 = 1;

/// x86_64, and 32-bit x86 programs on it (int 0x80).
const X86_64: &[ArchRules] = &[
    ArchRules {
        arch: 0xc000_003e,                            // AUDIT_ARCH_X86_64
        rules: &[(41, 0, AF_UNIX), (16, 1, TIOCSTI)], // socket, ioctl
        x32: true,
    },
    ArchRules {
        arch: 0x4000_0003,                                                   // AUDIT_ARCH_I386
        rules: &[(359, 0, AF_UNIX), (102, 0, SYS_SOCKET), (54, 1, TIOCSTI)], // socket, socketcall, ioctl
        x32: false,
    },
];

/// aarch64, and 32-bit ARM programs on it.
const AARCH64: &[ArchRules] = &[
    ArchRules {
        arch: 0xc000_00b7,                             // AUDIT_ARCH_AARCH64
        rules: &[(198, 0, AF_UNIX), (29, 1, TIOCSTI)], // socket, ioctl
        x32: false,
    },
    ArchRules {
        arch: 0x4000_0028,                                                   // AUDIT_ARCH_ARM
        rules: &[(281, 0, AF_UNIX), (102, 0, SYS_SOCKET), (54, 1, TIOCSTI)], // socket, socketcall, ioctl
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

/// The git folder and the shared git folder of `cwd` (one when they're the same).
pub fn git_dirs(cwd: &Path) -> Vec<PathBuf> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(cwd)
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
        for dir in [
            ".ssh",
            ".cache",
            ".novacode",
            ".config/gh",
            "extra",
            "proj-secret",
            ".cargo/bin",
            ".codex",
        ] {
            std::fs::create_dir_all(home.join(dir)).unwrap();
        }
        std::fs::create_dir_all(proj.join("sub")).unwrap();
        std::fs::create_dir_all(proj.join(".git/hooks")).unwrap();
        for f in [
            ".novacode/auth.json",
            ".novacode/auth-work.json",
            ".novacode/settings.json",
            ".netrc",
            ".config/gh/hosts.yml",
            ".codex/auth.json",
        ] {
            std::fs::write(home.join(f), "x").unwrap();
        }
        for f in [".env.secret", ".git/config", ".mcp.json"] {
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
            // Writable, parents first; missing ones (`~/missing`, `~/.npm`, …) skipped.
            ("--bind", "/tmp".to_string()),
            ("--bind", r("proj")),
            ("--bind", r("home/.cache")),
            ("--bind", r("home/.cargo")),
            ("--bind", r("home/extra")),
            // Read-only inside them: git config and hooks, agent config, tool binaries;
            // missing ones (`config.worktree`, `.claude`, `~/.rustup`, …) skipped.
            ("--ro-bind", r("proj/.git/config")),
            ("--ro-bind", r("proj/.git/hooks")),
            ("--ro-bind", r("proj/.mcp.json")),
            ("--ro-bind", r("home/.cargo/bin")),
            // Hidden: folders get a tmpfs, files /dev/null; `.aws` and `~/nope` don't exist.
            ("--tmpfs", r("home/.ssh")),
            ("/dev/null", r("home/.novacode/auth.json")),
            ("/dev/null", r("home/.novacode/auth-work.json")),
            ("/dev/null", r("home/.netrc")),
            ("/dev/null", r("home/.config/gh/hosts.yml")),
            ("/dev/null", r("home/.codex/auth.json")),
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
    fn vanished_paths_are_skipped() {
        let (d, c) = setup();
        let root = std::fs::canonicalize(d.path()).unwrap();
        let s = Sandbox::new(c);
        // Deleted mid-session: a writable folder, a read-only one, a hidden file and folder.
        std::fs::remove_dir(root.join("home/extra")).unwrap();
        std::fs::remove_dir(root.join("home/.cargo/bin")).unwrap();
        std::fs::remove_file(root.join("home/.netrc")).unwrap();
        std::fs::remove_dir(root.join("home/.ssh")).unwrap();
        let a = strs(&s.args(&root.join("proj")));
        for gone in ["home/extra", "home/.cargo/bin", "home/.netrc", "home/.ssh"] {
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
        std::fs::create_dir_all(&home).unwrap();
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
