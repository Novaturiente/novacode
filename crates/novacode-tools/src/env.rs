//! `ExecutionEnv` (L5.2): every file and process access goes through it, so an
//! SSH or sandbox backend can replace the local one later (L8.11, L6.5).

use crate::sandbox::Sandbox;
use futures_util::future::BoxFuture;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, UNIX_EPOCH};
use tokio::io::AsyncReadExt;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileMeta {
    pub len: u64,
    pub mtime_ms: u64,
    pub is_dir: bool,
}

#[derive(Debug, Clone)]
pub struct ExecSpec {
    pub command: String,
    pub cwd: PathBuf,
    pub env: Vec<(String, String)>,
    pub timeout: Duration,
    /// Run outside the sandbox (the `unsandboxed` argument, L6.5).
    pub unsandboxed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExecResult {
    /// `None` when killed by a signal.
    pub code: Option<i32>,
    pub timed_out: bool,
    pub cancelled: bool,
}

pub trait ExecutionEnv: Send + Sync {
    fn read<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, io::Result<Vec<u8>>>;
    /// Create parent dirs, then replace the file atomically (L0.5).
    fn write<'a>(&'a self, path: &'a Path, bytes: &'a [u8]) -> BoxFuture<'a, io::Result<()>>;
    fn remove<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, io::Result<()>>;
    /// `None` when the path doesn't exist.
    fn stat<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, io::Result<Option<FileMeta>>>;
    /// Real path, or the path itself when it doesn't exist yet (queue key).
    fn canonical<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, PathBuf>;
    /// Run a shell command in its own process group; merged stdout+stderr
    /// chunks go to `out`. Timeout and cancel kill the whole group.
    fn exec<'a>(
        &'a self,
        spec: ExecSpec,
        cancel: CancellationToken,
        out: mpsc::Sender<Vec<u8>>,
    ) -> BoxFuture<'a, io::Result<ExecResult>>;
    /// The sandbox model commands run in (L6.5), also for the process manager.
    fn sandbox(&self) -> Option<&Sandbox> {
        None
    }
}

pub struct LocalEnv;

/// [`LocalEnv`] whose commands run in the sandbox (L6.5).
pub struct SandboxedEnv(pub Arc<Sandbox>);

impl ExecutionEnv for SandboxedEnv {
    fn read<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, io::Result<Vec<u8>>> {
        LocalEnv.read(path)
    }
    fn write<'a>(&'a self, path: &'a Path, bytes: &'a [u8]) -> BoxFuture<'a, io::Result<()>> {
        LocalEnv.write(path, bytes)
    }
    fn remove<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, io::Result<()>> {
        LocalEnv.remove(path)
    }
    fn stat<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, io::Result<Option<FileMeta>>> {
        LocalEnv.stat(path)
    }
    fn canonical<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, PathBuf> {
        LocalEnv.canonical(path)
    }
    fn exec<'a>(
        &'a self,
        spec: ExecSpec,
        cancel: CancellationToken,
        out: mpsc::Sender<Vec<u8>>,
    ) -> BoxFuture<'a, io::Result<ExecResult>> {
        let sb = (!spec.unsandboxed).then_some(self.0.as_ref());
        Box::pin(run(spec, sb, cancel, out))
    }
    fn sandbox(&self) -> Option<&Sandbox> {
        Some(&self.0)
    }
}

impl ExecutionEnv for LocalEnv {
    fn read<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, io::Result<Vec<u8>>> {
        Box::pin(tokio::fs::read(path))
    }

    fn write<'a>(&'a self, path: &'a Path, bytes: &'a [u8]) -> BoxFuture<'a, io::Result<()>> {
        let (p, b) = (path.to_path_buf(), bytes.to_vec());
        Box::pin(async move {
            tokio::task::spawn_blocking(move || {
                // Write through an existing symlink to its target (a dangling one is still replaced).
                let p = std::fs::canonicalize(&p).unwrap_or(p);
                // Keep the file's mode (scripts stay executable); no .bak clutter in user repos,
                // and a user's own `<file>.bak` is never touched.
                let mode = std::fs::metadata(&p).ok().map(|m| m.permissions());
                novacode_base::storage::write_atomic_no_backup(&p, &b, novacode_base::storage::Durability::Fast)?;
                if let Some(m) = mode {
                    std::fs::set_permissions(&p, m)?;
                }
                Ok(())
            })
            .await
            .map_err(io::Error::other)?
        })
    }

    fn remove<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, io::Result<()>> {
        Box::pin(tokio::fs::remove_file(path))
    }

    fn stat<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, io::Result<Option<FileMeta>>> {
        Box::pin(async move {
            match tokio::fs::metadata(path).await {
                Ok(m) => Ok(Some(FileMeta {
                    len: m.len(),
                    mtime_ms: m
                        .modified()
                        .ok()
                        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                        .map_or(0, |d| d.as_millis() as u64),
                    is_dir: m.is_dir(),
                })),
                Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
                Err(e) => Err(e),
            }
        })
    }

    fn canonical<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, PathBuf> {
        Box::pin(async move {
            tokio::fs::canonicalize(path)
                .await
                .unwrap_or_else(|_| path.to_path_buf())
        })
    }

    fn exec<'a>(
        &'a self,
        spec: ExecSpec,
        cancel: CancellationToken,
        out: mpsc::Sender<Vec<u8>>,
    ) -> BoxFuture<'a, io::Result<ExecResult>> {
        Box::pin(run(spec, None, cancel, out))
    }
}

async fn run(
    spec: ExecSpec,
    sandbox: Option<&Sandbox>,
    cancel: CancellationToken,
    out: mpsc::Sender<Vec<u8>>,
) -> io::Result<ExecResult> {
    let mut child = crate::sandbox::command(sandbox, &spec.command, &spec.cwd)?
        .envs(spec.env.iter().map(|(k, v)| (k, v)))
        // stdin is /dev/null so nothing can wait on input.
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    let pgid = child.id().map(|p| p as i32);
    let pump = |mut r: Box<dyn tokio::io::AsyncRead + Unpin + Send>, tx: mpsc::Sender<Vec<u8>>| async move {
        let mut buf = vec![0u8; 16 * 1024];
        while let Ok(n) = r.read(&mut buf).await {
            if n == 0 || tx.send(buf[..n].to_vec()).await.is_err() {
                break;
            }
        }
    };
    let so = tokio::spawn(pump(Box::new(child.stdout.take().expect("piped")), out.clone()));
    let se = tokio::spawn(pump(Box::new(child.stderr.take().expect("piped")), out));
    let (mut timed_out, mut cancelled) = (false, false);
    let status = tokio::select! {
        s = child.wait() => Some(s?),
        _ = tokio::time::sleep(spec.timeout) => { timed_out = true; None }
        _ = cancel.cancelled() => { cancelled = true; None }
    };
    let status = match status {
        Some(s) => s,
        None => {
            kill_group(pgid);
            child.wait().await?
        }
    };
    // Background children may keep the pipes open; don't wait on them forever.
    let drain = async {
        let _ = so.await;
        let _ = se.await;
    };
    if tokio::time::timeout(Duration::from_millis(500), drain).await.is_err() {
        kill_group(pgid);
    }
    Ok(ExecResult {
        code: status.code(),
        timed_out,
        cancelled,
    })
}

fn kill_group(pgid: Option<i32>) {
    if let Some(p) = pgid {
        let _ = nix::sys::signal::killpg(nix::unistd::Pid::from_raw(p), nix::sys::signal::Signal::SIGKILL);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[tokio::test]
    async fn write_follows_symlinks_keeps_bak_and_mode() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path();
        // Through a symlink: the target changes, the link stays a link.
        std::fs::write(p.join("target.txt"), "old").unwrap();
        std::os::unix::fs::symlink("target.txt", p.join("link.txt")).unwrap();
        LocalEnv.write(&p.join("link.txt"), b"new").await.unwrap();
        assert_eq!(std::fs::read_to_string(p.join("target.txt")).unwrap(), "new");
        assert!(
            std::fs::symlink_metadata(p.join("link.txt"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
        // A user's own `f.bak` survives a write of `f`.
        std::fs::write(p.join("f"), "one").unwrap();
        std::fs::write(p.join("f.bak"), "mine").unwrap();
        LocalEnv.write(&p.join("f"), b"two").await.unwrap();
        assert_eq!(std::fs::read_to_string(p.join("f")).unwrap(), "two");
        assert_eq!(std::fs::read_to_string(p.join("f.bak")).unwrap(), "mine");
        // The mode is kept, and no .bak appears for a file without one.
        let s = p.join("run.sh");
        std::fs::write(&s, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&s, std::fs::Permissions::from_mode(0o755)).unwrap();
        LocalEnv.write(&s, b"#!/bin/sh\necho hi\n").await.unwrap();
        assert_eq!(std::fs::metadata(&s).unwrap().permissions().mode() & 0o777, 0o755);
        assert!(!p.join("run.sh.bak").exists());
    }
}
