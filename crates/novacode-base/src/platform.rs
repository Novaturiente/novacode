//! The OS-specific bits outside process kills (L11.14): home dir, shells,
//! process groups, file modes, dir links. Process kills live in
//! `novacode_tools::process`.

use std::fs::OpenOptions;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

/// The user's home dir: `$HOME`.
#[cfg(unix)]
pub fn home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

/// `%HOME%`, else `%USERPROFILE%`, as a full Windows path: Git Bash's `/c/Users/u`
/// becomes `c:/Users/u`, and 8.3 short names are expanded.
#[cfg(windows)]
pub fn home() -> Option<PathBuf> {
    let h = std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .or_else(|| std::env::var_os("USERPROFILE"))?;
    let s = h.to_string_lossy();
    let p = match s.as_bytes() {
        [b'/', d, rest @ ..] if d.is_ascii_alphabetic() && (rest.is_empty() || rest[0] == b'/') => {
            PathBuf::from(format!("{}:/{}", *d as char, s[2..].trim_start_matches('/')))
        }
        _ => PathBuf::from(&h),
    };
    Some(canonicalize(&p).unwrap_or(p))
}

/// The program for `bash -c CMD`.
#[cfg(unix)]
pub fn bash() -> PathBuf {
    "bash".into()
}

/// The program for `bash -c CMD`: `%NOVACODE_BASH%`, else Git for Windows' `bash.exe`.
/// Never `bash` from PATH: `C:\Windows\System32\bash.exe` is WSL's launcher.
#[cfg(windows)]
pub fn bash() -> PathBuf {
    static BASH: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    BASH.get_or_init(|| {
        if let Some(b) = std::env::var_os("NOVACODE_BASH").filter(|b| !b.is_empty()) {
            return b.into();
        }
        // `<Git>\cmd\git.exe` on PATH, then the default install folders.
        let from_path = std::env::var_os("PATH")
            .into_iter()
            .flat_map(|p| std::env::split_paths(&p).collect::<Vec<_>>())
            .filter(|d| d.join("git.exe").is_file())
            .filter_map(|d| d.parent().map(Path::to_path_buf));
        let defaults = ["ProgramFiles", "ProgramW6432", "LOCALAPPDATA"]
            .into_iter()
            .filter_map(std::env::var_os)
            .map(|d| {
                let d = PathBuf::from(d);
                if d.ends_with("Local") {
                    d.join("Programs").join("Git")
                } else {
                    d.join("Git")
                }
            });
        from_path
            .chain(defaults)
            .map(|g| g.join("bin").join("bash.exe"))
            .find(|b| b.is_file())
            // Not found: spawning fails with "program not found"; callers explain.
            .unwrap_or_else(|| "bash.exe".into())
    })
    .clone()
}

/// The program for `sh -c CMD`.
#[cfg(unix)]
pub fn sh() -> PathBuf {
    "sh".into()
}

/// Git Bash runs `sh -c` scripts too.
#[cfg(windows)]
pub fn sh() -> PathBuf {
    bash()
}

/// Start `c` in its own process group (pgid = pid), so a group kill reaches its children.
/// For a tokio command: `own_group(c.as_std_mut())`.
#[cfg(unix)]
pub fn own_group(c: &mut Command) -> &mut Command {
    use std::os::unix::process::CommandExt;
    c.process_group(0)
}

/// A new process group: the terminal's Ctrl+C doesn't reach it; novacode stops it itself.
#[cfg(windows)]
pub fn own_group(c: &mut Command) -> &mut Command {
    use std::os::windows::process::CommandExt;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    c.creation_flags(CREATE_NEW_PROCESS_GROUP)
}

/// Create the file `opts` opens with `mode` (`0o600` for secrets).
#[cfg(unix)]
pub fn set_create_mode(opts: &mut OpenOptions, mode: u32) -> &mut OpenOptions {
    use std::os::unix::fs::OpenOptionsExt;
    opts.mode(mode)
}

// shortcut: no owner-only ACL, files rely on the profile folder's default ACL; set one if shared machines matter.
#[cfg(windows)]
pub fn set_create_mode(opts: &mut OpenOptions, _mode: u32) -> &mut OpenOptions {
    opts
}

/// Set `path`'s permission bits (`0o700` for a private dir).
#[cfg(unix)]
pub fn set_mode(path: &Path, mode: u32) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
}

/// No Unix modes on Windows (see `set_create_mode`).
#[cfg(windows)]
pub fn set_mode(_path: &Path, _mode: u32) -> io::Result<()> {
    Ok(())
}

/// [`std::fs::canonicalize`], but on Windows without `\\?\` for plain drive paths:
/// git and bash can't read `\\?\C:\…`. Longer than 260 chars keeps it (the prefix is what lets that work).
pub fn canonicalize(p: impl AsRef<Path>) -> io::Result<PathBuf> {
    let c = std::fs::canonicalize(p)?;
    #[cfg(windows)]
    if let Some(s) = c.to_str().and_then(|s| s.strip_prefix(r"\\?\"))
        && s.as_bytes().get(1) == Some(&b':')
        && s.len() < 260
    {
        return Ok(PathBuf::from(s));
    }
    Ok(c)
}

/// `p` for splicing into a `bash -c` string: Git Bash reads `C:/x`, a `\` is an escape there.
pub fn slash_path(p: &Path) -> String {
    let s = p.display().to_string();
    if cfg!(windows) { s.replace('\\', "/") } else { s }
}

/// Remove the dir link made by [`link_dir`], not what it points at.
pub fn unlink_dir(link: &Path) -> io::Result<()> {
    // Windows removes a dir symlink or junction as a folder.
    if cfg!(windows) {
        std::fs::remove_dir(link)
    } else {
        std::fs::remove_file(link)
    }
}

/// Make `link` point at the dir `target`.
#[cfg(unix)]
pub fn link_dir(target: &Path, link: &Path) -> io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

/// A dir symlink needs Developer Mode or admin; a junction works for every user.
#[cfg(windows)]
pub fn link_dir(target: &Path, link: &Path) -> io::Result<()> {
    if std::os::windows::fs::symlink_dir(target, link).is_ok() {
        return Ok(());
    }
    let ok = Command::new("cmd")
        .arg("/C")
        .arg("mklink")
        .arg("/J")
        .arg(link)
        .arg(target)
        .stdout(std::process::Stdio::null())
        .status()?
        .success();
    if ok {
        Ok(())
    } else {
        Err(io::Error::other(format!("could not link {}", link.display())))
    }
}
