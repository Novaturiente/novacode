//! Subscription quota windows (L8.15, L11.4): 5-hour and weekly usage of the
//! Claude and Antigravity logins. Every novacode process shares one cache file
//! per provider, so an endpoint is hit at most once per [`MAX_AGE`].

use crate::auth::{AuthStore, DEFAULT_ACCOUNT};
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

fn path(home: &Path, provider: &str) -> PathBuf {
    home.join("cache").join("usage").join(format!("{provider}.json"))
}

/// Whether `provider` has quota windows we know how to read.
pub fn supported(provider: &str) -> bool {
    matches!(provider, "claude" | "antigravity")
}

fn win(v: &Value, pct: f64) -> Option<Window> {
    Some(Window {
        used_pct: pct.clamp(0.0, 100.0),
        resets_at: v.as_str().map(String::from),
    })
}

/// The windows for `model` from a provider's raw answer.
fn windows(provider: &str, model: &str, data: &Value) -> (Option<Window>, Option<Window>) {
    match provider {
        "claude" => {
            let w = |k: &str| {
                let x = &data[k];
                x["utilization"].as_f64().and_then(|u| win(&x["resets_at"], u))
            };
            (w("five_hour"), w("seven_day"))
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
                return (None, None);
            };
            let pick = |w: &str| {
                let b = buckets.iter().find(|b| b["window"].as_str() == Some(w))?;
                let rem = b["remainingFraction"].as_f64()?;
                win(&b["resetTime"], ((1.0 - rem) * 100.0).round())
            };
            (pick("5h"), pick("weekly"))
        }
        _ => (None, None),
    }
}

async fn fetch(auth: &AuthStore, provider: &str) -> Result<Value, String> {
    let cred = auth
        .credential(provider, DEFAULT_ACCOUNT)
        .await
        .map_err(|e| e.to_string())?;
    let http = crate::http::client();
    let req = match provider {
        "claude" => http
            .get("https://api.anthropic.com/api/oauth/usage")
            .bearer_auth(&cred.access)
            .header("anthropic-beta", "oauth-2025-04-20"),
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

/// Quota for `provider`'s default login, from the shared cache, refetched when
/// older than [`MAX_AGE`]. `None` for providers without quota windows.
pub async fn cached(home: &Path, auth: &AuthStore, provider: &str, model: &str) -> Option<Quota> {
    if !supported(provider) {
        return None;
    }
    let file = path(home, provider);
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
        match fetch(auth, provider).await {
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
    let (five_hour, weekly) = c.data.as_ref().map(|d| windows(provider, model, d)).unwrap_or_default();
    Some(Quota {
        provider: provider.into(),
        five_hour,
        weekly,
        at_ms: c.at_ms,
        error: c.error,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn reads_both_providers() {
        let claude = json!({"five_hour": {"utilization": 17.0, "resets_at": "2026-10-18T12:49:59Z"}, "seven_day": {"utilization": 54.0}});
        let (h, w) = windows("claude", "claude-opus-5-5", &claude);
        assert_eq!(h.unwrap().used_pct, 17.0);
        assert_eq!(w.unwrap().used_pct, 54.0);
        let ag = json!({"groups": [
            {"displayName": "Gemini Models", "buckets": [{"window": "5h", "remainingFraction": 0.75}, {"window": "weekly", "remainingFraction": 0.9}]},
            {"displayName": "Claude and GPT", "buckets": [{"window": "5h", "remainingFraction": 0.2, "resetTime": "t"}]}
        ]});
        let (h, w) = windows("antigravity", "gemini-3.8-flash-low", &ag);
        assert_eq!((h.unwrap().used_pct, w.unwrap().used_pct), (25.0, 10.0));
        let (h, w) = windows("antigravity", "claude-sonnet-4-6", &ag);
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
