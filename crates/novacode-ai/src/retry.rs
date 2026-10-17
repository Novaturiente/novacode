//! Retry policy (L2.18), pi's SDK-style policy: honour `x-should-retry`,
//! retry 408/409/429/5xx and network errors, prefer the server's delay,
//! otherwise back off min(0.5·2^i, 8)s with up to 25% jitter.

use reqwest::header::HeaderMap;
use std::time::Duration;

pub const MAX_RETRIES: u32 = 2;
/// A server asking for a longer wait than this fails instead of hanging.
pub const MAX_RETRY_DELAY: Duration = Duration::from_secs(60);

/// Whether a failed attempt (status `None` = network error) should be retried.
pub fn should_retry(status: Option<u16>, headers: Option<&HeaderMap>) -> bool {
    match headers
        .and_then(|h| h.get("x-should-retry"))
        .and_then(|v| v.to_str().ok())
    {
        Some("true") => return true,
        Some("false") => return false,
        _ => {}
    }
    match status {
        None => true,
        Some(s) => matches!(s, 408 | 409 | 429) || s >= 500,
    }
}

/// Server-requested delay from `retry-after-ms` or `retry-after` (seconds).
pub fn retry_after(headers: &HeaderMap) -> Option<Duration> {
    let get = |k| headers.get(k).and_then(|v| v.to_str().ok());
    if let Some(ms) = get("retry-after-ms").and_then(|v| v.trim().parse::<f64>().ok()) {
        return Some(Duration::from_secs_f64(ms.max(0.0) / 1000.0));
    }
    // shortcut: HTTP-date form of retry-after is ignored (falls back to backoff); add if a provider sends it.
    get("retry-after")
        .and_then(|v| v.trim().parse::<f64>().ok())
        .map(|s| Duration::from_secs_f64(s.max(0.0)))
}

/// Delay before retry number `attempt` (0-based), or `None` to give up.
pub fn delay(attempt: u32, server: Option<Duration>) -> Option<Duration> {
    if attempt >= MAX_RETRIES {
        return None;
    }
    if let Some(d) = server {
        return (d <= MAX_RETRY_DELAY).then_some(d);
    }
    let base = (0.5 * 2f64.powi(attempt as i32)).min(8.0);
    let mut b = [0u8; 2];
    let _ = getrandom::fill(&mut b);
    let jitter = 1.0 - 0.25 * (u16::from_le_bytes(b) as f64 / u16::MAX as f64);
    Some(Duration::from_secs_f64(base * jitter))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn policy() {
        assert!(should_retry(None, None));
        assert!(should_retry(Some(529), None));
        assert!(should_retry(Some(429), None));
        assert!(!should_retry(Some(400), None));
        let mut h = HeaderMap::new();
        h.insert("x-should-retry", "false".parse().unwrap());
        assert!(!should_retry(Some(503), Some(&h)));
        h.insert("retry-after", "3".parse().unwrap());
        assert_eq!(retry_after(&h), Some(Duration::from_secs(3)));
        h.insert("retry-after-ms", "250".parse().unwrap());
        assert_eq!(retry_after(&h), Some(Duration::from_millis(250)));
        assert_eq!(delay(0, Some(Duration::from_secs(61))), None);
        assert_eq!(delay(MAX_RETRIES, None), None);
        let d = delay(1, None).unwrap();
        assert!(d <= Duration::from_secs(1) && d >= Duration::from_millis(750), "{d:?}");
    }
}
