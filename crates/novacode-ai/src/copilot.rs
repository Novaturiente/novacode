//! GitHub Copilot (L2.9): one subscription, three wire formats. Claude models
//! go over Anthropic Messages, GPT/Grok/MAI over Responses, the rest over Chat
//! Completions, all at the token's API host with Copilot's headers.

use crate::adapter::{Adapter, Request};
use crate::anthropic::{self, Anthropic};
use crate::auth::copilot::{HEADERS, base_url};
use crate::openai_chat::{self, OpenAiChat};
use crate::openai_responses::{self, OpenAiResponses};
use crate::trace::Trace;
use futures_util::future::BoxFuture;
use novacode_types::{Block, Message, StreamEvent};
use serde_json::Value;
use std::collections::BTreeMap;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

pub const PROVIDER: &str = crate::auth::copilot::PROVIDER;

#[derive(Debug, PartialEq)]
pub enum Wire {
    Anthropic,
    Responses,
    Chat,
}

/// Which endpoint serves a model (pi's routing table).
pub fn wire(model: &str) -> Wire {
    let claude = ["claude-haiku-", "claude-sonnet-", "claude-opus-", "claude-fable-"]
        .iter()
        .any(|p| {
            model
                .strip_prefix(p)
                .is_some_and(|v| v.starts_with('4') || v.starts_with('5'))
        });
    if claude {
        Wire::Anthropic
    } else if ["gpt-3", "gpt-4"].iter().any(|p| model.starts_with(p)) {
        // shortcut: routed by id; switch to /models `supported_endpoints` if ids stop matching.
        Wire::Chat
    } else if ["gpt-", "grok-", "oswe", "mai-"].iter().any(|p| model.starts_with(p)) {
        Wire::Responses
    } else {
        Wire::Chat
    }
}

pub struct Copilot {
    http: reqwest::Client,
    token: String,
    base_url: String,
    trace: Option<Trace>,
}

impl Copilot {
    pub fn new(token: String, trace: Option<Trace>) -> Self {
        Self {
            http: crate::http::client(),
            base_url: base_url(&token),
            token,
            trace,
        }
    }

    /// Static headers plus the per-request ones. `x-initiator: agent` marks a
    /// follow-up after tool results, which Copilot doesn't bill as a new premium request.
    fn headers(&self, req: &Request) -> BTreeMap<String, String> {
        let mut h: BTreeMap<String, String> = HEADERS.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        let user = matches!(req.messages.last(), Some(Message::User { .. }));
        h.insert("x-initiator".into(), if user { "user" } else { "agent" }.into());
        h.insert("openai-intent".into(), "conversation-edits".into());
        let images = req.messages.iter().any(|m| match m {
            Message::User { content } | Message::ToolResult { content, .. } => {
                content.iter().any(|b| matches!(b, Block::Image { .. }))
            }
            _ => false,
        });
        if images {
            h.insert("copilot-vision-request".into(), "true".into());
        }
        h
    }

    async fn models(&self) -> Result<Vec<String>, String> {
        let mut r = self.http.get(format!("{}/models", self.base_url));
        for (k, v) in HEADERS {
            r = r.header(*k, *v);
        }
        let resp = r
            .bearer_auth(&self.token)
            .header("x-github-api-version", "2026-06-01")
            .timeout(Duration::from_secs(20))
            .send()
            .await
            .map_err(|e| e.to_string())?;
        let status = resp.status();
        let bytes = resp.bytes().await.map_err(|e| e.to_string())?;
        if !status.is_success() {
            return Err(format!("HTTP {status}: {}", String::from_utf8_lossy(&bytes)));
        }
        let v: Value = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
        Ok(usable(&v))
    }
}

/// Chat models the account may use: shown in the picker and not disabled,
/// or (some individual accounts flag none for the picker) explicitly enabled.
fn usable(v: &Value) -> Vec<String> {
    let chat: Vec<&Value> = v["data"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|m| m["capabilities"]["type"].as_str().unwrap_or("chat") == "chat")
        .collect();
    let pick = |f: &dyn Fn(&Value) -> bool| -> Vec<String> {
        let mut ids: Vec<String> = chat
            .iter()
            .filter(|m| f(m))
            .filter_map(|m| m["id"].as_str().map(String::from))
            .collect();
        ids.sort();
        ids.dedup();
        ids
    };
    let picker = pick(&|m| m["model_picker_enabled"] == true && m["policy"]["state"] != "disabled");
    if picker.is_empty() {
        pick(&|m| m["policy"]["state"] == "enabled")
    } else {
        picker
    }
}

impl Adapter for Copilot {
    fn stream(&self, req: Request, cancel: CancellationToken) -> mpsc::Receiver<StreamEvent> {
        let h = self.headers(&req);
        let trace = self.trace.clone();
        match wire(&req.model) {
            Wire::Anthropic => Anthropic::new(anthropic::Auth::Bearer(self.token.clone()), trace)
                .with_base_url(self.base_url.clone())
                .with_provider(PROVIDER)
                .with_headers(h)
                .with_http(self.http.clone())
                .stream(req, cancel),
            Wire::Responses => OpenAiResponses::new(
                PROVIDER,
                &self.base_url,
                openai_responses::Auth::ApiKey(self.token.clone()),
                openai_responses::Compat {
                    no_effort_none: true,
                    ..Default::default()
                },
                trace,
            )
            .with_headers(h)
            .with_http(self.http.clone())
            .stream(req, cancel),
            Wire::Chat => {
                let compat = openai_chat::Compat {
                    reasoning_effort: Some(false),
                    ..Default::default()
                };
                OpenAiChat::new(PROVIDER, &self.base_url, Some(self.token.clone()), compat, trace)
                    .with_headers(h)
                    .with_http(self.http.clone())
                    .stream(req, cancel)
            }
        }
    }

    fn list_models(&self) -> BoxFuture<'_, Result<Vec<String>, String>> {
        Box::pin(self.models())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn routing() {
        assert_eq!(wire("claude-sonnet-4.6"), Wire::Anthropic);
        assert_eq!(wire("claude-opus-5.5"), Wire::Anthropic);
        assert_eq!(wire("gpt-5-mini"), Wire::Responses);
        assert_eq!(wire("gpt-4.1"), Wire::Chat);
        assert_eq!(wire("gpt-4o-mini"), Wire::Chat);
        assert_eq!(wire("grok-4.7"), Wire::Responses);
        assert_eq!(wire("gemini-3.8-flash"), Wire::Chat);
        assert_eq!(wire("kimi-k3"), Wire::Chat);
    }

    #[test]
    fn usable_models() {
        let v = json!({ "data": [
            { "id": "gpt-5-mini", "model_picker_enabled": true, "policy": { "state": "enabled" } },
            { "id": "claude-x", "model_picker_enabled": true, "policy": { "state": "disabled" } },
            { "id": "embed", "model_picker_enabled": true, "capabilities": { "type": "embeddings" } },
            { "id": "old", "model_picker_enabled": false },
        ]});
        assert_eq!(usable(&v), vec!["gpt-5-mini"]);
        let v = json!({ "data": [{ "id": "a", "policy": { "state": "enabled" } }, { "id": "b" }] });
        assert_eq!(usable(&v), vec!["a"]);
    }
}
