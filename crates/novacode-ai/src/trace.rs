//! `--trace` (L2.24): raw request and SSE events as JSONL, credentials redacted.
//! These files double as replay fixtures (L11.5).

use crate::sse::SseEvent;
use novacode_base::storage::append_jsonl;
use serde_json::json;
use std::path::PathBuf;

const SECRET_HEADERS: &[&str] = &["authorization", "x-api-key", "cookie"];

#[derive(Debug, Clone)]
pub struct Trace {
    pub path: PathBuf,
}

impl Trace {
    fn write(&self, v: serde_json::Value) {
        if let Err(e) = append_jsonl(&self.path, &v) {
            tracing::warn!(path = %self.path.display(), error = %e, "trace write failed");
        }
    }

    pub fn request(&self, req: &reqwest::RequestBuilder, body: &[u8]) {
        let Some(built) = req.try_clone().and_then(|r| r.build().ok()) else {
            return;
        };
        let headers: serde_json::Map<String, serde_json::Value> = built
            .headers()
            .iter()
            .map(|(k, v)| {
                let v = if SECRET_HEADERS.contains(&k.as_str()) {
                    "<redacted>"
                } else {
                    v.to_str().unwrap_or("<binary>")
                };
                (k.to_string(), json!(v))
            })
            .collect();
        let body: serde_json::Value = serde_json::from_slice(body).unwrap_or_default();
        self.write(json!({ "kind": "request", "url": built.url().as_str(), "headers": headers, "body": body }));
    }

    pub fn response_error(&self, status: u16, body: &str) {
        self.write(json!({ "kind": "http_error", "status": status, "body": body }));
    }

    pub fn sse(&self, ev: &SseEvent) {
        self.write(json!({ "kind": "sse", "event": ev.event, "data": ev.data }));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_credentials() {
        let dir = tempfile::tempdir().unwrap();
        let t = Trace {
            path: dir.path().join("t.jsonl"),
        };
        let req = reqwest::Client::new()
            .post("http://x/v1")
            .header("x-api-key", "sk-secret")
            .bearer_auth("tok");
        t.request(&req, b"{\"a\":1}");
        let text = std::fs::read_to_string(&t.path).unwrap();
        assert!(!text.contains("sk-secret") && !text.contains("tok\""), "{text}");
        assert!(text.contains("<redacted>") && text.contains("\"a\":1"));
    }
}
