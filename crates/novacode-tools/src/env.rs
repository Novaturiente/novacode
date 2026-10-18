//! `ExecutionEnv` (L5.2): every file and process access goes through it, so an
//! SSH or sandbox backend can replace the local one later (L8.11, L6.5).

use futures_util::future::BoxFuture;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Stdio;
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
}

pub struct LocalEnv;

impl ExecutionEnv for LocalEnv {
    fn read<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, io::Result<Vec<u8>>> {
        Box::pin(tokio::fs::read(path))
    }

    fn write<'a>(&'a self, path: &'a Path, bytes: &'a [u8]) -> BoxFuture<'a, io::Result<()>> {
        let (p, b) = (path.to_path_buf(), bytes.to_vec());
        Box::pin(async move {
            tokio::task::spawn_blocking(move || {
                // Keep the file's mode (scripts stay executable); no .bak clutter in user repos.
                let mode = std::fs::metadata(&p).ok().map(|m| m.permissions());
                novacode_base::storage::write_atomic(&p, &b, novacode_base::storage::Durability::Fast)?;
                let _ = std::fs::remove_file(novacode_base::storage::backup_path(&p));
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
        Box::pin(async move {
            let mut child = tokio::process::Command::new("bash")
                .arg("-c")
                .arg(&spec.command)
                .current_dir(&spec.cwd)
                .envs(spec.env.iter().map(|(k, v)| (k, v)))
                // stdin is /dev/null so nothing can wait on input; a /dev/tty prompt still hangs until the timeout.
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .process_group(0)
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
        })
    }
}

fn kill_group(pgid: Option<i32>) {
    if let Some(p) = pgid {
        let _ = nix::sys::signal::killpg(nix::unistd::Pid::from_raw(p), nix::sys::signal::Signal::SIGKILL);
    }
}
