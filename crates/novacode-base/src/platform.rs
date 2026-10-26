//! The OS-specific bits outside process kills (L11.14): home dir, shells,
//! process groups, file modes, dir links. Process kills live in
//! `novacode_tools::process`.

use std::fs::OpenOptions;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

/// The user's home dir: `$HOME`.
pub fn home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

/// The program for `bash -c CMD`.
pub fn bash() -> PathBuf {
    "bash".into()
}

/// The program for `sh -c CMD`.
pub fn sh() -> PathBuf {
    "sh".into()
}

/// Start `c` in its own process group (pgid = pid), so a group kill reaches its children.
/// For a tokio command: `own_group(c.as_std_mut())`.
pub fn own_group(c: &mut Command) -> &mut Command {
    use std::os::unix::process::CommandExt;
    c.process_group(0)
}

/// Create the file `opts` opens with `mode` (`0o600` for secrets).
pub fn set_create_mode(opts: &mut OpenOptions, mode: u32) -> &mut OpenOptions {
    use std::os::unix::fs::OpenOptionsExt;
    opts.mode(mode)
}

/// Set `path`'s permission bits (`0o700` for a private dir).
pub fn set_mode(path: &Path, mode: u32) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
}

/// Make `link` point at the dir `target`.
pub fn link_dir(target: &Path, link: &Path) -> io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}
