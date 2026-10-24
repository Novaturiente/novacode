//! OpenAI Chat Completions adapter (L2.6): the wire most vendors speak.
//! Vendor differences are compat flags in the profile (L2.7), not code paths.

use crate::adapter::{Adapter, Builder, Request};
use crate::catalog::effort_name;
use crate::http::{self, Failure, Wire};
use crate::sse::SseEvent;
use crate::trace::Trace;
use crate::transform::{self, Target};
use futures_util::future::BoxFuture;
use novacode_types::{Block, BlockKind, ErrorKind, Message, StopReason, StreamEvent, ThinkingLevel, Usage};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// Vendor quirks (profile `compat`, overridable in settings).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Compat {
    /// `max_tokens` (default) or `max_completion_tokens`.
    pub max_tokens_field: Option<String>,
    /// Send instructions as `developer` instead of `system`.
    pub developer_role: bool,
    /// How thinking is switched on: `openai` (default, `reasoning_effort`),
    /// `openrouter`, `deepseek`, `zai`, `qwen` or `none`.
    pub thinking_format: Option<String>,
    /// For `openai` format: whether `reasoning_effort` is accepted (default true).
    pub reasoning_effort: Option<bool>,
    /// Always send `reasoning_content` on assistant messages (DeepSeek, Kimi).
    pub reasoning_content_on_assistant: bool,
    /// Replay earlier thinking as plain text.
    pub thinking_as_text: bool,
    /// Tool results carry the tool `name`.
    pub tool_result_name: bool,
    /// `stream_options.include_usage` (default true).
    pub stream_usage: Option<bool>,
    /// `anthropic`: put `cache_control` markers on content parts (OpenRouter → Claude/Gemini).
    pub cache_control: Option<String>,
    /// The API takes `system` messages mid-conversation; otherwise later ones
    /// become a `<system-reminder>` user message, so the prefix stays put.
    pub mid_convo_system: bool,
    /// Send `prompt_cache_key` = session id (L2.17).
    pub prompt_cache_key: bool,
}

pub struct OpenAiChat {
    http: reqwest::Client,
    base_url: String,
    key: Option<String>,
    headers: BTreeMap<String, String>,
    compat: Compat,
    /// Replay namespace = the profile id: one vendor's reasoning isn't sent to another.
    provider: String,
    trace: Option<Trace>,
}

impl OpenAiChat {
    pub fn new(provider: &str, base_url: &str, key: Option<String>, compat: Compat, trace: Option<Trace>) -> Self {
        Self {
            http: http::client(),
            base_url: base_url.trim_end_matches('/').into(),
            key,
            headers: BTreeMap::new(),
            compat,
            provider: provider.into(),
            trace,
        }
    }

    pub fn with_headers(mut self, h: BTreeMap<String, String>) -> Self {
        self.headers = h;
        self
    }

    /// Share a connection pool (one per provider, not one per request).
    pub fn with_http(mut self, c: reqwest::Client) -> Self {
        self.http = c;
        self
    }

    fn authed(&self, mut r: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        if let Some(k) = &self.key {
            r = r.bearer_auth(k);
        }
        for (k, v) in &self.headers {
            r = r.header(k, v);
        }
        r
    }

    async fn models(&self) -> Result<Vec<String>, String> {
        let resp = self
            .authed(self.http.get(format!("{}/models", self.base_url)))
            .timeout(Duration::from_secs(20))
            .send()
            .await
            .map_err(|e| e.to_string())?;
        let status = resp.status();
        let bytes = resp.bytes().await.map_err(|e| e.to_string())?;
        if !status.is_success() {
            return Err(
                http::classify_openai(status.as_u16(), &Default::default(), &String::from_utf8_lossy(&bytes)).1,
            );
        }
        let body: Value = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
        let mut ids: Vec<String> = body["data"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|m| m["id"].as_str().map(String::from))
            .collect();
        ids.sort();
        Ok(ids)
    }
}

impl Adapter for OpenAiChat {
    fn stream(&self, req: Request, cancel: CancellationToken) -> mpsc::Receiver<StreamEvent> {
        let use_key = self.base_url.contains("api.openai.com") || self.compat.prompt_cache_key;
        let body = build_body(&req, &self.compat, &self.provider, use_key);
        let body = serde_json::to_vec(&body).unwrap_or_default();
        let request = self
            .authed(self.http.post(format!("{}/chat/completions", self.base_url)))
            .header("content-type", "application/json")
            .header("accept", "text/event-stream")
            .body(body.clone());
        let (mut b, rx) = http::channel(&self.provider, &req.model);
        b.thinking_level = req
            .thinking
            .filter(|l| *l != ThinkingLevel::Off)
            .map(|l| effort_name(l).into());
        http::drive(b, rx, request, body, self.trace.clone(), cancel, State::default)
    }

    fn list_models(&self) -> BoxFuture<'_, Result<Vec<String>, String>> {
        Box::pin(self.models())
    }
}

// ---------- request ----------

/// The JSON body. Pure and deterministic (L1.7 rule 3).
pub fn build_body(req: &Request, c: &Compat, ns: &str, cache_key: bool) -> Value {
    let history = transform::for_target(
        &req.messages_with_tail(),
        &Target {
            provider: ns,
            model: &req.model,
            images: req.images,
            normalize_id: Some(transform::safe_id),
        },
    );
    let mut messages = convert(&history, c, ns, &req.model);
    let mut body = Map::new();
    body.insert("model".into(), json!(req.model));
    let claude_like = req.model.contains("claude") || req.model.contains("gemini");
    if c.cache_control.as_deref() == Some("anthropic") && claude_like {
        mark_cache(&mut messages);
    }
    body.insert("messages".into(), Value::Array(messages));
    if !req.tools.is_empty() {
        let tools: Vec<Value> = req
            .tools
            .iter()
            .map(|t| json!({ "type": "function", "function": { "name": t.name, "description": t.description, "parameters": t.schema } }))
            .collect();
        body.insert("tools".into(), Value::Array(tools));
    }
    let field = c.max_tokens_field.as_deref().unwrap_or("max_tokens");
    body.insert(field.into(), json!(req.max_tokens));
    // `thinking` is only set for reasoning models (L2.16): Off switches reasoning off where
    // the format has a documented switch.
    // shortcut: the plain `openai` format sends nothing for Off (vendors differ on `none`).
    if req.thinking == Some(ThinkingLevel::Off) {
        match c.thinking_format.as_deref().unwrap_or("openai") {
            "openrouter" => {
                body.insert("reasoning".into(), json!({ "effort": "none" }));
            }
            "deepseek" | "zai" => {
                body.insert("thinking".into(), json!({ "type": "disabled" }));
            }
            "qwen" => {
                body.insert("enable_thinking".into(), json!(false));
            }
            _ => {}
        }
    }
    if let Some(level) = req.thinking.filter(|l| *l != ThinkingLevel::Off) {
        let effort = effort_name(level);
        match c.thinking_format.as_deref().unwrap_or("openai") {
            "openrouter" => {
                body.insert("reasoning".into(), json!({ "effort": effort }));
            }
            "deepseek" => {
                body.insert("thinking".into(), json!({ "type": "enabled" }));
            }
            "zai" => {
                body.insert("thinking".into(), json!({ "type": "enabled", "clear_thinking": false }));
            }
            "qwen" => {
                body.insert("enable_thinking".into(), json!(true));
            }
            "none" => {}
            _ if c.reasoning_effort != Some(false) => {
                body.insert("reasoning_effort".into(), json!(effort));
            }
            _ => {}
        }
    }
    if cache_key && let Some(s) = &req.session {
        body.insert("prompt_cache_key".into(), json!(s.chars().take(64).collect::<String>()));
    }
    body.insert("stream".into(), json!(true));
    if c.stream_usage != Some(false) {
        body.insert("stream_options".into(), json!({ "include_usage": true }));
    }
    Value::Object(body)
}

fn convert(history: &[Message], c: &Compat, ns: &str, model: &str) -> Vec<Value> {
    let role = if c.developer_role { "developer" } else { "system" };
    let mut out: Vec<Value> = Vec::new();
    let mut leading = true;
    let mut tool_images: Vec<Value> = Vec::new();
    let flush = |out: &mut Vec<Value>, imgs: &mut Vec<Value>| {
        if !imgs.is_empty() {
            let mut parts = vec![json!({ "type": "text", "text": "Images from the tool results above:" })];
            parts.append(imgs);
            out.push(json!({ "role": "user", "content": parts }));
        }
    };
    for m in history {
        if !matches!(m, Message::ToolResult { .. }) {
            flush(&mut out, &mut tool_images);
        }
        match m {
            Message::System { text } if text.is_empty() => {}
            Message::System { text } if leading => match out.last_mut() {
                // One system message for the whole leading prompt.
                Some(last) if last["role"] == role => {
                    let joined = format!("{}\n\n{text}", last["content"].as_str().unwrap_or_default());
                    last["content"] = json!(joined);
                }
                _ => out.push(json!({ "role": role, "content": text })),
            },
            Message::System { text } if c.mid_convo_system => out.push(json!({ "role": role, "content": text })),
            Message::System { text } => out.push(json!({
                "role": "user",
                "content": format!("<system-reminder>\n{text}\n</system-reminder>"),
            })),
            Message::Custom { .. } => {}
            Message::User { content } => {
                leading = false;
                if let Some(v) = user_content(content) {
                    out.push(json!({ "role": "user", "content": v }));
                }
            }
            Message::Assistant(a) => {
                leading = false;
                if let Some(v) = assistant(a, c, ns, model) {
                    out.push(v);
                }
            }
            Message::ToolResult {
                call_id,
                tool_name,
                content,
                ..
            } => {
                let text: Vec<&str> = content
                    .iter()
                    .filter_map(|b| match b {
                        Block::Text { text, .. } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect();
                tool_images.extend(content.iter().filter_map(image_part));
                let text = if text.is_empty() {
                    "(no output)".to_string()
                } else {
                    text.join("\n")
                };
                let mut r = json!({ "role": "tool", "tool_call_id": call_id, "content": text });
                if c.tool_result_name {
                    r["name"] = json!(tool_name);
                }
                out.push(r);
            }
        }
    }
    flush(&mut out, &mut tool_images);
    out
}

fn image_part(b: &Block) -> Option<Value> {
    match b {
        Block::Image { mime_type, data } => {
            Some(json!({ "type": "image_url", "image_url": { "url": format!("data:{mime_type};base64,{data}") } }))
        }
        _ => None,
    }
}

/// Plain string for text-only content (some vendors take nothing else), parts with images.
fn user_content(content: &[Block]) -> Option<Value> {
    let has_image = content.iter().any(|b| matches!(b, Block::Image { .. }));
    let texts: Vec<&str> = content
        .iter()
        .filter_map(|b| match b {
            Block::Text { text, .. } if !text.is_empty() => Some(text.as_str()),
            _ => None,
        })
        .collect();
    if !has_image {
        return (!texts.is_empty()).then(|| json!(texts.join("\n")));
    }
    let parts: Vec<Value> = content
        .iter()
        .filter_map(|b| match b {
            Block::Text { text, .. } if !text.is_empty() => Some(json!({ "type": "text", "text": text })),
            b => image_part(b),
        })
        .collect();
    Some(Value::Array(parts))
}

fn assistant(a: &novacode_types::AssistantMessage, c: &Compat, ns: &str, model: &str) -> Option<Value> {
    let same = a.provider == ns && a.model == model;
    let mut text = String::new();
    let mut thinking: Vec<(&str, &Option<novacode_types::Replay>)> = Vec::new();
    let mut calls = Vec::new();
    for b in &a.content {
        match b {
            Block::Text { text: t, .. } => text.push_str(t),
            Block::Thinking { text, replay } if same && !text.trim().is_empty() => thinking.push((text, replay)),
            Block::Thinking { replay, .. } if same && replay.is_some() => thinking.push(("", replay)),
            Block::ToolCall(call) => calls.push(json!({
                "id": call.id,
                "type": "function",
                // Raw arguments as received (serde_json keeps key order), so the bytes don't change.
                "function": { "name": call.name, "arguments": call.arguments.to_string() },
            })),
            _ => {}
        }
    }
    let mut m = Map::new();
    m.insert("role".into(), json!("assistant"));
    let thought: String = thinking
        .iter()
        .map(|(t, _)| *t)
        .filter(|t| !t.is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    if c.thinking_as_text && !thought.is_empty() {
        text = format!("{thought}\n\n{text}");
    } else if !thinking.is_empty() {
        let data = thinking
            .iter()
            .find_map(|(_, r)| r.as_ref().filter(|r| r.provider == ns).map(|r| &r.data));
        let field = data.and_then(|d| d["field"].as_str()).unwrap_or("reasoning_content");
        if !thought.is_empty() {
            m.insert(field.into(), json!(thought));
        }
        if let Some(d) = data.and_then(|d| d.get("details")) {
            m.insert("reasoning_details".into(), d.clone());
        }
    }
    if c.reasoning_content_on_assistant && !m.contains_key("reasoning_content") {
        m.insert("reasoning_content".into(), json!(""));
    }
    if text.is_empty() && calls.is_empty() {
        return None;
    }
    m.insert(
        "content".into(),
        if text.is_empty() { Value::Null } else { json!(text) },
    );
    if !calls.is_empty() {
        m.insert("tool_calls".into(), Value::Array(calls));
    }
    Some(Value::Object(m))
}

/// Anthropic-style markers through OpenRouter: end of the leading system
/// message, the last message, and the previous turn's last assistant message.
fn mark_cache(messages: &mut [Value]) {
    let n = messages.len();
    let sys = messages
        .iter()
        .take_while(|m| m["role"] == "system" || m["role"] == "developer")
        .count();
    let prev = messages[..n.saturating_sub(1)]
        .iter()
        .rposition(|m| m["role"] == "assistant");
    for i in [sys.checked_sub(1), prev, n.checked_sub(1)].into_iter().flatten() {
        let m = &mut messages[i];
        if let Some(s) = m["content"].as_str().map(String::from) {
            m["content"] = json!([{ "type": "text", "text": s }]);
        }
        if let Some(last) = m["content"].as_array_mut().and_then(|a| a.last_mut()) {
            last["cache_control"] = json!({ "type": "ephemeral" });
        }
    }
}

// ---------- stream ----------

#[derive(Default)]
struct State {
    next: usize,
    text: Option<usize>,
    thinking: Option<(usize, String)>,
    tools: BTreeMap<u64, usize>,
    /// For vendors that omit `index`: tool call id to its key, and the last key opened.
    ids: BTreeMap<String, u64>,
    last_key: Option<u64>,
    details: Vec<Value>,
    finish: Option<StopReason>,
    usage: Usage,
}

impl State {
    async fn close_text(&mut self, b: &mut Builder) {
        if let Some(i) = self.text.take() {
            b.end(i).await;
        }
    }
    async fn close_thinking(&mut self, b: &mut Builder) {
        if let Some((i, field)) = self.thinking.take() {
            let mut data = json!({ "field": field });
            if !self.details.is_empty() {
                data["details"] = Value::Array(std::mem::take(&mut self.details));
            }
            b.set_replay(i, data);
            b.end(i).await;
        }
    }
    async fn close_all(&mut self, b: &mut Builder) {
        if !self.details.is_empty() && self.thinking.is_none() {
            let i = self.next;
            self.next += 1;
            b.start(i, BlockKind::Thinking, "", "").await;
            self.thinking = Some((i, "reasoning".into()));
        }
        self.close_thinking(b).await;
        self.close_text(b).await;
        for (_, i) in std::mem::take(&mut self.tools) {
            b.end(i).await;
        }
    }
    fn stop(&self) -> StopReason {
        match &self.finish {
            // Some vendors say "stop" even when they called tools.
            Some(StopReason::Stop) | None if !self.tools.is_empty() => StopReason::ToolUse,
            Some(s) => s.clone(),
            None => StopReason::Stop,
        }
    }
}

impl Wire for State {
    fn classify(status: u16, headers: &reqwest::header::HeaderMap, body: &str) -> Failure {
        http::classify_openai(status, headers, body)
    }

    async fn at_eof(&mut self, b: &mut Builder) -> Result<StopReason, Failure> {
        if self.finish.is_none() {
            return Err((ErrorKind::Transient, "stream ended before a finish reason".into()));
        }
        let stop = self.stop();
        self.close_all(b).await;
        Ok(stop)
    }

    async fn on_event(&mut self, ev: &SseEvent, b: &mut Builder) -> Result<Option<StopReason>, Failure> {
        let data = ev.data.trim();
        if data.is_empty() {
            return Ok(None);
        }
        if data == "[DONE]" {
            let stop = self.stop();
            self.close_all(b).await;
            return Ok(Some(stop));
        }
        let v: Value =
            serde_json::from_str(data).map_err(|e| (ErrorKind::Transient, format!("bad event json: {e}")))?;
        if v.get("error").is_some() {
            // Gateways send overloads inside HTTP 200: take the status from the event.
            let code = v["error"]["code"].as_u64().filter(|c| (400..600).contains(c));
            let (kind, msg) = http::classify_openai(code.unwrap_or(500) as u16, &Default::default(), data);
            // An error event mid-stream is the server's fault unless it says otherwise.
            let kind = if kind == ErrorKind::InvalidRequest && b.started {
                ErrorKind::Transient
            } else {
                kind
            };
            return Err((kind, msg));
        }
        if b.response_id.is_none() {
            b.response_id = v["id"].as_str().map(String::from);
        }
        if let Some(u) = v.get("usage").filter(|u| u.is_object()) {
            let get = |p: &str| u.pointer(p).and_then(Value::as_u64);
            let prompt = get("/prompt_tokens").unwrap_or(0);
            let cached = get("/prompt_tokens_details/cached_tokens")
                .or(get("/prompt_cache_hit_tokens"))
                .unwrap_or(0);
            self.usage = Usage {
                input: prompt.saturating_sub(cached),
                output: get("/completion_tokens").unwrap_or(0),
                cache_read: cached,
                cache_write: get("/prompt_tokens_details/cache_write_tokens").unwrap_or(0),
                reasoning: get("/completion_tokens_details/reasoning_tokens"),
            };
            // Anthropic-style cache writes come on top of prompt tokens on some routers.
            self.usage.input = self.usage.input.saturating_sub(self.usage.cache_write);
            b.usage(self.usage).await;
        }
        let Some(choice) = v["choices"].get(0) else {
            return Ok(None);
        };
        let d = &choice["delta"];
        // Reasoning before content: some proxies send both in one delta.
        let reasoning = ["reasoning_content", "reasoning", "reasoning_text"]
            .into_iter()
            .find_map(|f| d[f].as_str().filter(|s| !s.is_empty()).map(|s| (f, s)));
        if let Some((field, text)) = reasoning {
            self.close_text(b).await;
            if self.thinking.is_none() {
                let i = self.next;
                self.next += 1;
                b.start(i, BlockKind::Thinking, "", "").await;
                self.thinking = Some((i, field.into()));
            }
            let i = self.thinking.as_ref().map(|t| t.0).unwrap_or_default();
            b.delta(i, text).await;
        }
        if let Some(details) = d["reasoning_details"].as_array() {
            self.details.extend(details.iter().cloned());
        }
        if let Some(text) = d["content"].as_str().filter(|s| !s.is_empty()) {
            self.close_thinking(b).await;
            if self.text.is_none() {
                let i = self.next;
                self.next += 1;
                b.start(i, BlockKind::Text, "", "").await;
                self.text = Some(i);
            }
            b.delta(self.text.unwrap_or_default(), text).await;
        }
        if let Some(calls) = d["tool_calls"].as_array() {
            self.close_thinking(b).await;
            self.close_text(b).await;
            for (n, tc) in calls.iter().enumerate() {
                let id = tc["id"].as_str().filter(|s| !s.is_empty());
                // Without `index`, a new id is a new call; no id continues the last one.
                let key = match (tc["index"].as_u64(), id) {
                    (Some(k), _) => k,
                    (None, Some(id)) => match self.ids.get(id) {
                        Some(k) => *k,
                        None => {
                            let k = (0..).find(|k| !self.tools.contains_key(k)).unwrap_or_default();
                            self.ids.insert(id.into(), k);
                            k
                        }
                    },
                    (None, None) => self.last_key.unwrap_or(n as u64),
                };
                self.last_key = Some(key);
                let i = match self.tools.get(&key) {
                    Some(i) => *i,
                    None => {
                        let i = self.next;
                        self.next += 1;
                        let id = tc["id"]
                            .as_str()
                            .map(String::from)
                            .unwrap_or_else(|| format!("call_{i}"));
                        let name = tc["function"]["name"].as_str().unwrap_or_default();
                        b.start(i, BlockKind::ToolCall, &id, name).await;
                        self.tools.insert(key, i);
                        i
                    }
                };
                if let Some(args) = tc["function"]["arguments"].as_str().filter(|s| !s.is_empty()) {
                    b.delta(i, args).await;
                }
            }
        }
        if let Some(f) = choice["finish_reason"].as_str() {
            self.finish = Some(match f {
                "length" => StopReason::Length,
                "tool_calls" | "function_call" => StopReason::ToolUse,
                _ => StopReason::Stop, // stop, content_filter, end_turn
            });
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter::{CacheRetention, collect};
    use novacode_types::{AssistantMessage, Replay, ToolCall, ToolDef};

    fn req(messages: Vec<Message>) -> Request {
        Request {
            tail: Vec::new(),
            model: "m".into(),
            messages,
            tools: vec![ToolDef {
                name: "read".into(),
                description: "Read".into(),
                schema: json!({"type":"object"}),
            }],
            max_tokens: 100,
            cache: CacheRetention::Short,
            thinking: Some(ThinkingLevel::High),
            images: true,
            session: Some("sess".into()),
        }
    }

    fn history() -> Vec<Message> {
        vec![
            Message::System {
                text: "be brief".into(),
            },
            Message::user("hi"),
            Message::Assistant(AssistantMessage {
                content: vec![
                    Block::Thinking {
                        text: "plan".into(),
                        replay: Some(Replay {
                            provider: "ds".into(),
                            data: json!({"field":"reasoning_content"}),
                        }),
                    },
                    Block::ToolCall(ToolCall {
                        id: "c1".into(),
                        name: "read".into(),
                        arguments: json!({"path":"a"}),
                        arguments_error: None,
                        replay: None,
                    }),
                ],
                stop: StopReason::ToolUse,
                usage: Usage::default(),
                provider: "ds".into(),
                model: "m".into(),
                response_id: None,
                error_message: None,
                thinking_level: None,
            }),
            Message::ToolResult {
                call_id: "c1".into(),
                tool_name: "read".into(),
                content: vec![
                    Block::text("A"),
                    Block::Image {
                        mime_type: "image/png".into(),
                        data: "QQ".into(),
                    },
                ],
                is_error: false,
                details: None,
            },
            Message::System {
                text: "branch changed".into(),
            },
        ]
    }

    #[test]
    fn body_shape_and_compat() {
        let c: Compat =
            serde_json::from_value(json!({ "thinkingFormat": "deepseek", "reasoningContentOnAssistant": true }))
                .unwrap();
        let b = build_body(&req(history()), &c, "ds", false);
        let m = b["messages"].as_array().unwrap();
        assert_eq!(m[0], json!({ "role": "system", "content": "be brief" }));
        assert_eq!(m[1], json!({ "role": "user", "content": "hi" }));
        assert_eq!(
            m[2]["reasoning_content"], "plan",
            "same vendor+model: thinking replayed"
        );
        assert_eq!(m[2]["tool_calls"][0]["function"]["arguments"], r#"{"path":"a"}"#);
        assert_eq!(m[3], json!({ "role": "tool", "tool_call_id": "c1", "content": "A" }));
        assert_eq!(
            m[4]["content"][1]["image_url"]["url"], "data:image/png;base64,QQ",
            "tool image follows results"
        );
        assert_eq!(m[5]["content"], "<system-reminder>\nbranch changed\n</system-reminder>");
        assert_eq!(b["thinking"], json!({ "type": "enabled" }));
        assert_eq!(b["max_tokens"], 100);
        assert!(b.get("prompt_cache_key").is_none());
        // Another vendor: no reasoning replay, but the field DeepSeek requires is there.
        let b = build_body(&req(history()), &c, "other", true);
        assert_eq!(b["messages"][2]["reasoning_content"], "");
        assert_eq!(b["prompt_cache_key"], "sess");
        let plain = build_body(&req(history()), &Compat::default(), "x", false);
        assert_eq!(plain["reasoning_effort"], "high");
        assert!(plain["messages"][2].get("reasoning_content").is_none());
    }

    #[test]
    fn thinking_off_per_format() {
        let off = |format: &str| {
            let mut r = req(vec![Message::user("hi")]);
            r.thinking = Some(ThinkingLevel::Off);
            let c = Compat {
                thinking_format: Some(format.into()),
                ..Default::default()
            };
            let b = build_body(&r, &c, "x", false);
            (
                b["reasoning"].clone(),
                b["thinking"].clone(),
                b["enable_thinking"].clone(),
                b["reasoning_effort"].clone(),
            )
        };
        assert_eq!(off("openrouter").0, json!({ "effort": "none" }));
        assert_eq!(off("deepseek").1, json!({ "type": "disabled" }));
        assert_eq!(off("zai").1, json!({ "type": "disabled" }));
        assert_eq!(off("qwen").2, json!(false));
        assert_eq!(off("openai"), (Value::Null, Value::Null, Value::Null, Value::Null));
    }

    #[test]
    fn openrouter_cache_markers() {
        let c: Compat =
            serde_json::from_value(json!({ "thinkingFormat": "openrouter", "cacheControl": "anthropic" })).unwrap();
        let mut r = req(history());
        r.model = "anthropic/claude-sonnet-5".into();
        let b = build_body(&r, &c, "openrouter", false);
        let m = b["messages"].as_array().unwrap();
        assert_eq!(m[0]["content"][0]["cache_control"]["type"], "ephemeral");
        assert_eq!(m.last().unwrap()["content"][0]["cache_control"]["type"], "ephemeral");
        assert_eq!(b["reasoning"], json!({ "effort": "high" }));
    }

    async fn run_stream(events: &[&str]) -> novacode_types::AssistantMessage {
        let (mut b, rx) = http::channel("ds", "m");
        let mut st = State::default();
        let mut stop = None;
        for e in events {
            let ev = SseEvent {
                event: String::new(),
                data: e.to_string(),
            };
            if let Some(s) = st.on_event(&ev, &mut b).await.unwrap() {
                stop = Some(s);
                break;
            }
        }
        let stop = match stop {
            Some(s) => s,
            None => st.at_eof(&mut b).await.unwrap(),
        };
        b.finish(stop).await;
        collect(rx).await
    }

    #[tokio::test]
    async fn stream_reasoning_text_and_tools() {
        let m = run_stream(&[
            r#"{"id":"r1","choices":[{"delta":{"reasoning_content":"think "}}]}"#,
            r#"{"choices":[{"delta":{"reasoning_content":"more"}}]}"#,
            r#"{"choices":[{"delta":{"content":"Hello"}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_a","function":{"name":"read","arguments":"{\"pa"}}]}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"th\":\"x\"}"}}]}}]}"#,
            r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
            r#"{"choices":[],"usage":{"prompt_tokens":100,"completion_tokens":20,"prompt_tokens_details":{"cached_tokens":80}}}"#,
            "[DONE]",
        ])
        .await;
        assert_eq!(m.stop, StopReason::ToolUse);
        assert_eq!(m.response_id.as_deref(), Some("r1"));
        assert_eq!((m.usage.input, m.usage.cache_read, m.usage.output), (20, 80, 20));
        assert!(
            matches!(&m.content[0], Block::Thinking { text, replay: Some(r) } if text == "think more" && r.data["field"] == "reasoning_content")
        );
        assert_eq!(m.content[1], Block::text("Hello"));
        let call = m.tool_calls().next().unwrap();
        assert_eq!((call.id.as_str(), &call.arguments), ("call_a", &json!({"path":"x"})));
    }

    #[tokio::test]
    async fn stream_without_done_and_stop_with_tools() {
        let m = run_stream(&[
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c","function":{"name":"ls","arguments":"{}"}}]},"finish_reason":"stop"}]}"#,
        ])
        .await;
        assert_eq!(m.stop, StopReason::ToolUse, "stop with tool calls means tool use");
        let (mut b, _rx) = http::channel("ds", "m");
        let mut st = State::default();
        st.on_event(
            &SseEvent {
                event: String::new(),
                data: r#"{"choices":[{"delta":{"content":"x"}}]}"#.into(),
            },
            &mut b,
        )
        .await
        .unwrap();
        assert_eq!(st.at_eof(&mut b).await.unwrap_err().0, ErrorKind::Transient);
    }

    #[tokio::test]
    async fn tool_calls_without_index_stay_apart() {
        let m = run_stream(&[
            r#"{"choices":[{"delta":{"tool_calls":[{"id":"a","function":{"name":"ls","arguments":"{\"p"}}]}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"function":{"arguments":"\":1}"}}]}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"id":"b","function":{"name":"read","arguments":"{}"}}]}}]}"#,
            r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
        ])
        .await;
        let calls: Vec<_> = m.tool_calls().map(|c| (c.id.as_str(), c.arguments.clone())).collect();
        assert_eq!(calls, [("a", json!({"p":1})), ("b", json!({}))]);
    }

    #[tokio::test]
    async fn mid_stream_error_uses_its_code() {
        let err = |data: &str| {
            let ev = SseEvent {
                event: String::new(),
                data: data.into(),
            };
            async move {
                let (mut b, _rx) = http::channel("ds", "m");
                State::default().on_event(&ev, &mut b).await.unwrap_err().0
            }
        };
        assert_eq!(
            err(r#"{"error":{"code":503,"message":"overloaded"}}"#).await,
            ErrorKind::Transient
        );
        assert_eq!(
            err(r#"{"error":{"message":"upstream failed"}}"#).await,
            ErrorKind::Transient,
            "no code: a server error"
        );
        assert_eq!(
            err(r#"{"error":{"code":429,"message":"slow down"}}"#).await,
            ErrorKind::RateLimit { retry_after_ms: None }
        );
        assert_eq!(
            err(r#"{"error":{"code":400,"message":"bad field"}}"#).await,
            ErrorKind::InvalidRequest,
            "before any output a 400 stays a request error"
        );
    }
}
