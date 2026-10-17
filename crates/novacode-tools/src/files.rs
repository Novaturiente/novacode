//! Per-realpath mutation queue (L5.10) and the stale-file guard (L5.6).

use crate::ToolCtx;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tokio::sync::OwnedMutexGuard;

#[derive(Default)]
pub struct FileState {
    locks: Mutex<HashMap<PathBuf, Arc<tokio::sync::Mutex<()>>>>,
    /// mtime the agent last saw for each real path.
    seen: Mutex<HashMap<PathBuf, u64>>,
}

/// Holds one file's queue slot.
pub struct FileGuard {
    pub path: PathBuf,
    _guard: OwnedMutexGuard<()>,
}

impl FileState {
    /// Wait for the file's slot. Mutations of one real path run one at a time.
    pub async fn lock(&self, ctx: &ToolCtx, path: &Path) -> FileGuard {
        let real = ctx.env.canonical(path).await;
        let m = {
            let mut locks = self.locks.lock().unwrap();
            locks.retain(|_, m| Arc::strong_count(m) > 1); // bounded: idle entries are dropped
            locks.entry(real.clone()).or_default().clone()
        };
        FileGuard {
            path: real,
            _guard: m.lock_owned().await,
        }
    }

    pub fn saw(&self, real: &Path, mtime_ms: u64) {
        self.seen.lock().unwrap().insert(real.to_path_buf(), mtime_ms);
    }

    /// Error text if the file changed on disk since the agent last read or wrote it.
    pub async fn check_stale(&self, ctx: &ToolCtx, real: &Path) -> Option<String> {
        let seen = *self.seen.lock().unwrap().get(real)?;
        let now = ctx.env.stat(real).await.ok().flatten()?.mtime_ms;
        (now != seen).then(|| {
            format!(
                "{} changed on disk since you last read it. Read it again before editing.",
                real.display()
            )
        })
    }

    /// Record the current mtime after a read or write.
    pub async fn record(&self, ctx: &ToolCtx, real: &Path) {
        if let Ok(Some(m)) = ctx.env.stat(real).await {
            self.saw(real, m.mtime_ms);
        }
    }
}
