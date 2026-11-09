//! The UI's clock: elapsed times, ages and the double-click window read it.
//! Tests freeze it (per thread), so screens with times are the same on every run.

use std::cell::Cell;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

thread_local! {
    /// (monotonic, wall-clock ms) while frozen.
    static FROZEN: Cell<Option<(Instant, u64)>> = const { Cell::new(None) };
}

fn wall_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// Now, for measuring durations.
pub fn now() -> Instant {
    FROZEN.with(Cell::get).map_or_else(Instant::now, |(i, _)| i)
}

/// Wall-clock milliseconds since the Unix epoch, for ages of things the host timestamps.
pub fn now_ms() -> u64 {
    FROZEN.with(Cell::get).map_or_else(wall_ms, |(_, ms)| ms)
}

/// Time since `at` (zero if `at` is later).
pub fn since(at: Instant) -> Duration {
    now().saturating_duration_since(at)
}

/// Tests: stop this thread's clock at the current time.
#[doc(hidden)]
pub fn freeze() {
    FROZEN.with(|f| f.set(Some((Instant::now(), wall_ms()))));
}

/// Tests: move this thread's frozen clock forward (freezes it first if needed).
#[doc(hidden)]
pub fn advance(d: Duration) {
    if FROZEN.with(Cell::get).is_none() {
        freeze();
    }
    FROZEN.with(|f| f.set(f.get().map(|(i, ms)| (i + d, ms + d.as_millis() as u64))));
}
