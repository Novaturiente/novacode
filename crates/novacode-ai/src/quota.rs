//! Subscription quota windows (L8.15, L11.4): 5-hour and weekly usage of the
//! Claude and Antigravity logins. Every novacode process shares one cache file
//! per provider account, so an endpoint is hit at most once per [`MAX_AGE`].

use crate::auth::AuthStore;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const MAX_AGE: Duration = Duration::from_secs(5 * 60);

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Window {
    /// 0–100.
    pub used_pct: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resets_at: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Quota {
    pub provider: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub five_hour: Option<Window>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub weekly: Option<Window>,
    /// A ~30-day window (ChatGPT Go and similar plans).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub monthly: Option<Window>,
    /// When the data was fetched (unix ms): its age.
    pub at_ms: u64,
    /// The last fetch failed; the windows are from before.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Default, Serialize, Deserialize)]
struct Cache {
    #[serde(default)]
    data: Option<Value>,
    #[serde(default)]
    at_ms: u64,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    backoff_until_ms: u64,
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

/// `<provider>-<account>.json`; `account` `None` = `<provider>.json`, the copy of
/// whichever account a session used last, kept for readers from before accounts.
fn path(home: &Path, provider: &str, account: Option<&str>) -> PathBuf {
    let name = match account {
        Some(a) => format!("{provider}-{a}.json"),
        None => format!("{provider}.json"),
    };
    home.join("cache").join("usage").join(name)
}

/// Whether `provider` has quota windows we know how to read.
pub fn supported(provider: &str) -> bool {
    matches!(provider, "claude" | "antigravity" | "codex")
}

fn win(v: &Value, pct: f64) -> Option<Window> {
    Some(Window {
        used_pct: pct.clamp(0.0, 100.0),
        resets_at: v.as_str().map(String::from),
    })
}

impl Window {
    /// "in 2h13m", "in 4d03h", "now"; `None` without a readable reset time.
    pub fn resets_in(&self, now_ms: u64) -> Option<String> {
        let at = parse_iso(self.resets_at.as_deref()?)?;
        let secs = at - (now_ms / 1000) as i64;
        Some(match secs {
            ..=0 => "now".into(),
            1..3600 => format!("in {}m", (secs + 59) / 60),
            3600..86_400 => format!("in {}h{:02}m", secs / 3600, secs % 3600 / 60),
            _ => format!("in {}d{:02}h", secs / 86_400, secs % 86_400 / 3600),
        })
    }
}

/// `YYYY-MM-DDTHH:MM:SS[.frac](Z|±HH:MM)` as unix seconds.
fn parse_iso(s: &str) -> Option<i64> {
    let n = |r: std::ops::Range<usize>| s.get(r)?.parse::<i64>().ok();
    let (y, mo, d, h, mi, se) = (n(0..4)?, n(5..7)?, n(8..10)?, n(11..13)?, n(14..16)?, n(17..19)?);
    let rest = s[19..].trim_start_matches(|c: char| c == '.' || c.is_ascii_digit());
    let off = match rest.as_bytes().first() {
        Some(b'+' | b'-') => {
            let sign = if rest.starts_with('-') { -1 } else { 1 };
            sign * (rest.get(1..3)?.parse::<i64>().ok()? * 3600 + rest.get(4..6)?.parse::<i64>().ok()? * 60)
        }
        _ => 0,
    };
    // days_from_civil (Howard Hinnant)
    let y = if mo <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (mo + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(days * 86_400 + h * 3600 + mi * 60 + se - off)
}

/// The windows for `model` from a provider's raw answer: 5h, weekly, monthly.
fn windows(provider: &str, model: &str, data: &Value) -> (Option<Window>, Option<Window>, Option<Window>) {
    match provider {
        "claude" => {
            let w = |k: &str| {
                let x = &data[k];
                x["utilization"].as_f64().and_then(|u| win(&x["resets_at"], u))
            };
            (w("five_hour"), w("seven_day"), None)
        }
        "antigravity" => {
            // Gemini models and third-party ones (Claude, GPT) have separate buckets.
            let third = ["claude", "gpt", "sonnet", "opus"].iter().any(|k| model.contains(k));
            let groups = data["groups"].as_array().cloned().unwrap_or_default();
            let group = groups
                .iter()
                .find(|g| {
                    let n = g["displayName"].as_str().unwrap_or("").to_lowercase();
                    if third {
                        n.contains("claude") || n.contains("gpt")
                    } else {
                        n.contains("gemini")
                    }
                })
                .or(groups.first());
            let Some(buckets) = group.and_then(|g| g["buckets"].as_array()) else {
                return (None, None, None);
            };
            let pick = |w: &str| {
                let b = buckets.iter().find(|b| b["window"].as_str() == Some(w))?;
                let rem = b["remainingFraction"].as_f64()?;
                win(&b["resetTime"], ((1.0 - rem) * 100.0).round())
            };
            (pick("5h"), pick("weekly"), None)
        }
        "codex" => {
            // ChatGPT wham/usage: windows sorted by their length, which depends on the plan
            // (Plus: 5h + weekly; Go: one 30-day window).
            let mut out = (None, None, None);
            for k in ["primary_window", "secondary_window"] {
                let x = &data["rate_limit"][k];
                let reset = x["reset_at"].as_i64().map(|t| Value::String(iso_utc(t)));
                let Some(w) = x["used_percent"]
                    .as_f64()
                    .and_then(|u| win(reset.as_ref().unwrap_or(&Value::Null), u))
                else {
                    continue;
                };
                const DAY: u64 = 86_400;
                match x["limit_window_seconds"].as_u64().unwrap_or(5 * 3600) {
                    0..=21_600 => out.0 = Some(w),
                    s if (6 * DAY..=8 * DAY).contains(&s) => out.1 = Some(w),
                    s if (28 * DAY..=32 * DAY).contains(&s) => out.2 = Some(w),
                    _ => {}
                }
            }
            out
        }
        _ => (None, None, None),
    }
}

/// Unix seconds as `YYYY-MM-DDTHH:MM:SSZ` (Howard Hinnant's civil-from-days).
fn iso_utc(t: i64) -> String {
    let (days, sec) = (t.div_euclid(86_400), t.rem_euclid(86_400));
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        sec / 3600,
        sec % 3600 / 60,
        sec % 60
    )
}

async fn fetch(auth: &AuthStore, provider: &str, account: &str) -> Result<Value, String> {
    let cred = auth.credential(provider, account).await.map_err(|e| e.to_string())?;
    let http = crate::http::client();
    let req = match provider {
        "claude" => http
            .get("https://api.anthropic.com/api/oauth/usage")
            .bearer_auth(&cred.access)
            .header("anthropic-beta", "oauth-2025-04-20"),
        "codex" => {
            let mut r = http
                .get("https://chatgpt.com/backend-api/wham/usage")
                .bearer_auth(&cred.access)
                .header("accept", "application/json");
            if let Some(id) = crate::auth::codex::account_id(&cred.access) {
                r = r.header("chatgpt-account-id", id);
            }
            r
        }
        _ => http
            .post("https://daily-cloudcode-pa.googleapis.com/v1internal:retrieveUserQuotaSummary")
            .bearer_auth(&cred.access)
            .header("content-type", "application/json")
            .header("user-agent", crate::auth::antigravity::USER_AGENT)
            .body("{}"),
    };
    let resp = req
        .timeout(Duration::from_secs(10))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let status = resp.status();
    let bytes = resp.bytes().await.map_err(|e| e.to_string())?;
    if !status.is_success() {
        return Err(format!("HTTP {}", status.as_u16()));
    }
    serde_json::from_slice(&bytes).map_err(|e| e.to_string())
}

/// Quota for one login of `provider`, from the shared cache, refetched with that
/// account's token when older than [`MAX_AGE`]. `None` for providers without quota windows.
pub async fn cached(home: &Path, auth: &AuthStore, provider: &str, account: &str, model: &str) -> Option<Quota> {
    if !supported(provider) {
        return None;
    }
    let file = path(home, provider, Some(account));
    let mut c: Cache = novacode_base::storage::read_json(&file)
        .ok()
        .flatten()
        .unwrap_or_default();
    let now = now_ms();
    let fresh = c.data.is_some() && now.saturating_sub(c.at_ms) < MAX_AGE.as_millis() as u64;
    if !fresh && now >= c.backoff_until_ms {
        // Claim the fetch so other processes don't fetch at the same moment.
        c.backoff_until_ms = now + 15_000;
        let _ = std::fs::create_dir_all(file.parent().unwrap_or(home));
        let _ = novacode_base::storage::write_json(&file, &c, novacode_base::storage::Durability::Fast);
        match fetch(auth, provider, account).await {
            Ok(d) => {
                c.data = Some(d);
                c.at_ms = now_ms();
                c.error = None;
            }
            Err(e) => {
                c.backoff_until_ms = now_ms()
                    + if e.contains("429") {
                        MAX_AGE.as_millis() as u64
                    } else {
                        60_000
                    };
                c.error = Some(e);
            }
        }
        let _ = novacode_base::storage::write_json(&file, &c, novacode_base::storage::Durability::Fast);
    }
    // The account in use is the one `<provider>.json` readers mean.
    let _ = novacode_base::storage::write_json(
        &path(home, provider, None),
        &c,
        novacode_base::storage::Durability::Fast,
    );
    let (five_hour, weekly, monthly) = c.data.as_ref().map(|d| windows(provider, model, d)).unwrap_or_default();
    Some(Quota {
        provider: provider.into(),
        five_hour,
        weekly,
        monthly,
        at_ms: c.at_ms,
        error: c.error,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn each_account_has_its_own_cache_file() {
        let home = tempfile::tempdir().unwrap();
        let dir = home.path().join("cache").join("usage");
        std::fs::create_dir_all(&dir).unwrap();
        let fresh = |pct: f64| json!({"data": {"five_hour": {"utilization": pct}}, "at_ms": now_ms()});
        std::fs::write(dir.join("claude-default.json"), fresh(50.0).to_string()).unwrap();
        std::fs::write(dir.join("claude-mili.json"), fresh(10.0).to_string()).unwrap();
        let auth = AuthStore::new(home.path().join("auth.json"));
        // Fresh files: read, no fetch.
        let q = cached(home.path(), &auth, "claude", "mili", "m").await.unwrap();
        assert_eq!(q.five_hour.unwrap().used_pct, 10.0);
        let legacy: Value = serde_json::from_slice(&std::fs::read(dir.join("claude.json")).unwrap()).unwrap();
        assert_eq!(
            legacy["data"]["five_hour"]["utilization"], 10.0,
            "claude.json follows the account in use"
        );
        let q = cached(home.path(), &auth, "claude", "default", "m").await.unwrap();
        assert_eq!(q.five_hour.unwrap().used_pct, 50.0);
        assert_eq!(path(home.path(), "claude", Some("mili")), dir.join("claude-mili.json"));
    }

    #[test]
    fn reads_codex_windows() {
        let d = json!({"rate_limit": {
            "primary_window": {"used_percent": 12.0, "limit_window_seconds": 18000, "reset_at": 1_760_000_000},
            "secondary_window": {"used_percent": 40.5, "limit_window_seconds": 604800, "reset_at": 0}
        }});
        let (h, w, m) = windows("codex", "gpt-5.6-luna", &d);
        let h = h.unwrap();
        assert_eq!(
            (h.used_pct, h.resets_at.as_deref()),
            (12.0, Some("2025-10-09T08:53:20Z"))
        );
        assert_eq!(w.unwrap().resets_at.as_deref(), Some("1970-01-01T00:00:00Z"));
        assert!(m.is_none());
        // Go plan: a single 30-day window.
        let go = json!({"rate_limit": {"primary_window": {"used_percent": 3.0, "limit_window_seconds": 2592000, "reset_at": 1}, "secondary_window": null}});
        let (h, w, m) = windows("codex", "x", &go);
        assert!(h.is_none() && w.is_none() && m.unwrap().used_pct == 3.0);
    }

    #[test]
    fn reset_times_read_as_relative() {
        assert_eq!(parse_iso("2025-10-09T08:53:20Z"), Some(1_760_000_000));
        assert_eq!(parse_iso("2025-10-09T10:53:20.6589+02:00"), Some(1_760_000_000));
        let w = |s: &str| Window {
            used_pct: 0.0,
            resets_at: Some(s.into()),
        };
        let now = 1_760_000_000_000;
        assert_eq!(w("2025-10-09T11:06:20Z").resets_in(now).as_deref(), Some("in 2h13m"));
        assert_eq!(w("2025-10-13T11:53:20Z").resets_in(now).as_deref(), Some("in 4d03h"));
        assert_eq!(w("2020-01-01T00:00:00Z").resets_in(now).as_deref(), Some("now"));
    }

    #[test]
    fn reads_both_providers() {
        let claude = json!({"five_hour": {"utilization": 17.0, "resets_at": "2026-10-18T12:49:59Z"}, "seven_day": {"utilization": 54.0}});
        let (h, w, _) = windows("claude", "claude-opus-5-5", &claude);
        assert_eq!(h.unwrap().used_pct, 17.0);
        assert_eq!(w.unwrap().used_pct, 54.0);
        let ag = json!({"groups": [
            {"displayName": "Gemini Models", "buckets": [{"window": "5h", "remainingFraction": 0.75}, {"window": "weekly", "remainingFraction": 0.9}]},
            {"displayName": "Claude and GPT", "buckets": [{"window": "5h", "remainingFraction": 0.2, "resetTime": "t"}]}
        ]});
        let (h, w, _) = windows("antigravity", "gemini-3.8-flash-low", &ag);
        assert_eq!((h.unwrap().used_pct, w.unwrap().used_pct), (25.0, 10.0));
        let (h, w, _) = windows("antigravity", "claude-sonnet-4-6", &ag);
        assert_eq!(
            h.unwrap(),
            Window {
                used_pct: 80.0,
                resets_at: Some("t".into())
            }
        );
        assert!(w.is_none());
    }
}
