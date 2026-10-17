//! Logging (L0.7): daily-rotated files in `<home>/logs`, level from
//! `NOVACODE_LOG` (EnvFilter syntax), `trace` forces debug level.

use crate::paths::Paths;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::EnvFilter;

/// Keep the guard alive for the whole process; dropping it flushes the log.
pub fn init(paths: &Paths, trace: bool) -> std::io::Result<WorkerGuard> {
    std::fs::create_dir_all(paths.logs())?;
    let appender = tracing_appender::rolling::daily(paths.logs(), "novacode.log");
    let (writer, guard) = tracing_appender::non_blocking(appender);
    let default = if trace { "debug" } else { "info" };
    let filter = EnvFilter::try_from_env("NOVACODE_LOG").unwrap_or_else(|_| EnvFilter::new(default));
    // shortcut: repeated-warning rate limiting (L0.7) not built; add when a noisy warn shows up.
    tracing_subscriber::fmt()
        .with_writer(writer)
        .with_ansi(false)
        .with_env_filter(filter)
        .try_init()
        .map_err(std::io::Error::other)?;
    Ok(guard)
}
