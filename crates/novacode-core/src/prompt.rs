//! Minimal system prompt for M2. Named sections, context files and deltas arrive in M6 (L4.7).
//! Date and cwd are written once at session start, so they never break the cache.

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

pub fn system_prompt(cwd: &Path) -> String {
    format!(
        "You are novacode, a coding agent working in the user's terminal. Use the tools to read, search and edit files and to run commands. Read a file before editing it. Keep replies short and concrete.\n\nWorking directory: {}\nDate: {}",
        cwd.display(),
        today()
    )
}

/// UTC date as YYYY-MM-DD (civil-from-days, Howard Hinnant's algorithm).
fn today() -> String {
    let days = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() / 86_400) as i64;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}")
}

#[cfg(test)]
mod tests {
    #[test]
    fn date_matches_system_date() {
        let out = std::process::Command::new("date")
            .args(["-u", "+%Y-%m-%d"])
            .output()
            .unwrap();
        assert_eq!(super::today(), String::from_utf8_lossy(&out.stdout).trim());
    }
}
