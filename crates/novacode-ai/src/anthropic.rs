//! Anthropic Messages adapter (L2.5) with API-key and Claude subscription
//! (OAuth) profiles (L2.1), cache markers (L2.17), retry before content (L2.18),
//! overflow detection (L2.19) and redacted tracing (L2.24).

use crate::adapter::{Adapter, Builder, CacheRetention, Request};
use crate::http::{self, Failure, Wire};
use crate::retry;
use crate::sse::SseEvent;
use crate::trace::Trace;
use crate::transform::{self, Target};
use novacode_types::{Block, BlockKind, ErrorKind, Message, StopReason, StreamEvent, ThinkingLevel, Usage};
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

pub const PROVIDER: &str = "anthropic";
const API_VERSION: &str = "2023-06-01";

/// Vendor-controlled values for the Claude subscription. Bundled defaults;
/// overridable in settings because Anthropic changes them (L2.1).
#[derive(Debug, Clone)]
pub struct OAuthProfile {
    pub user_agent: String,
    pub betas: String,
    /// First system block, e.g. `cc_version=…; cc_entrypoint=sdk-cli; cch=…;`.
    pub billing_header: String,
    pub identity: String,
}

impl Default for OAuthProfile {
    fn default() -> Self {
        Self {
            user_agent: "claude-cli/2.1.294 (external, sdk-cli)".into(),
            betas: "claude-code-20250219,oauth-2025-04-20,interleaved-thinking-2025-05-14".into(),
            billing_header: "cc_version=2.1.294; cc_entrypoint=sdk-cli; cch=33f85;".into(),
            identity: "You are a Claude agent, built on Anthropic's Claude Agent SDK.".into(),
        }
    }
}

#[derive(Debug, Clone)]
pub enum Auth {
    ApiKey(String),
    /// Subscription access token (already refreshed by the caller).
    OAuth {
        access_token: String,
        profile: OAuthProfile,
    },
    /// A plain bearer token for an Anthropic-compatible gateway (GitHub Copilot).
    Bearer(String),
}

pub struct Anthropic {
    http: reqwest::Client,
    base_url: String,
    auth: Auth,
    trace: Option<Trace>,
    /// Replay namespace: `anthropic` for Claude (key or subscription), the vendor id otherwise.
    provider: String,
    headers: BTreeMap<String, String>,
}

impl Anthropic {
    pub fn new(auth: Auth, trace: Option<Trace>) -> Self {
        Self {
            http: http::client(),
            base_url: "https://api.anthropic.com".into(),
            auth,
            trace,
            provider: PROVIDER.into(),
            headers: BTreeMap::new(),
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

    /// An Anthropic-compatible vendor (e.g. MiniMax): its own replay namespace.
    pub fn with_provider(mut self, provider: impl Into<String>) -> Self {
        self.provider = provider.into();
        self
    }

    pub fn with_base_url(mut self, url: impl Into<String>) -> Self {
        self.base_url = url.into();
        self
    }

    /// Model ids this login or key can use, newest first (the live list, L2.10).
    pub async fn list_models(&self) -> Result<Vec<String>, String> {
        let oauth = matches!(self.auth, Auth::OAuth { .. });
        let url = format!(
            "{}/v1/models?limit=100{}",
            self.base_url,
            if oauth { "&beta=true" } else { "" }
        );
        let resp = self
            .authed(self.http.get(url), &[])
            .timeout(Duration::from_secs(20))
            .send()
            .await
            .map_err(|e| e.to_string())?;
        let status = resp.status();
        let bytes = resp.bytes().await.map_err(|e| e.to_string())?;
        let body: Value = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
        if !status.is_success() {
            return Err(format!(
                "HTTP {status}: {}",
                body["error"]["message"].as_str().unwrap_or("")
            ));
        }
        Ok(body["data"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|m| m["id"].as_str().map(String::from))
            .collect())
    }

    fn authed(&self, r: reqwest::RequestBuilder, extra_betas: &[&str]) -> reqwest::RequestBuilder {
        let r = r.header("anthropic-version", API_VERSION);
        let mut betas: Vec<&str> = match &self.auth {
            Auth::OAuth { profile, .. } => profile.betas.split(',').collect(),
            Auth::ApiKey(_) | Auth::Bearer(_) => Vec::new(),
        };
        betas.extend(extra_betas);
        let r = if betas.is_empty() {
            r
        } else {
            r.header("anthropic-beta", betas.join(","))
        };
        let mut r = match &self.auth {
            Auth::ApiKey(k) => r.header("x-api-key", k),
            Auth::Bearer(t) => r.bearer_auth(t),
            Auth::OAuth { access_token, profile } => r
                .bearer_auth(access_token)
                .header("user-agent", &profile.user_agent)
                .header("x-app", "cli"),
        };
        for (k, v) in &self.headers {
            r = r.header(k, v);
        }
        r
    }

    fn http_request(&self, body: &[u8], extra_betas: &[&str]) -> reqwest::RequestBuilder {
        let oauth = matches!(self.auth, Auth::OAuth { .. });
        let url = format!("{}/v1/messages{}", self.base_url, if oauth { "?beta=true" } else { "" });
        let r = self
            .http
            .post(url)
            .header("content-type", "application/json")
            .header("accept", "text/event-stream");
        self.authed(r, extra_betas).body(body.to_vec())
    }
}

impl Adapter for Anthropic {
    fn stream(&self, req: Request, cancel: CancellationToken) -> mpsc::Receiver<StreamEvent> {
        let oauth_profile = match &self.auth {
            Auth::OAuth { profile, .. } => Some(profile.clone()),
            Auth::ApiKey(_) | Auth::Bearer(_) => None,
        };
        let body = serde_json::to_vec(&build_body(&req, oauth_profile.as_ref(), &self.provider)).unwrap_or_default();
        let effort = in_conversation_effort(&req.model, req.thinking);
        // Mid-conversation effort needs these two betas (as pi sends them).
        let betas: &[&str] = if effort.is_some() {
            &[
                "mid-conversation-output-config-2026-07-01",
                "thinking-binding-controls-2026-08-01",
            ]
        } else {
            &[]
        };
        let request = self.http_request(&body, betas);
        let (mut b, rx) = http::channel(&self.provider, &req.model);
        b.thinking_level = effort;
        http::drive(b, rx, request, body, self.trace.clone(), cancel, State::default)
    }

    fn list_models(&self) -> futures_util::future::BoxFuture<'_, Result<Vec<String>, String>> {
        Box::pin(Anthropic::list_models(self))
    }
}

#[derive(Default)]
struct State {
    stop: Option<StopReason>,
    usage: Usage,
}

impl Wire for State {
    fn classify(status: u16, headers: &reqwest::header::HeaderMap, body: &str) -> Failure {
        classify(status, headers, body)
    }

    async fn at_eof(&mut self, _b: &mut Builder) -> Result<StopReason, Failure> {
        self.stop
            .clone()
            .ok_or((ErrorKind::Transient, "stream ended before message_stop".into()))
    }

    /// Handle one SSE event; `Some(stop)` ends the stream.
    async fn on_event(&mut self, ev: &SseEvent, b: &mut Builder) -> Result<Option<StopReason>, Failure> {
        let (event, data, st) = (ev.event.as_str(), ev.data.as_str(), self);
        if data.is_empty() {
            return Ok(None);
        }
        let v: Value =
            serde_json::from_str(data).map_err(|e| (ErrorKind::Transient, format!("bad event json: {e}")))?;
        let kind = v["type"].as_str().unwrap_or(event);
        let index = v["index"].as_u64().unwrap_or(0) as usize;
        match kind {
            "message_start" => {
                let m = &v["message"];
                b.response_id = m["id"].as_str().map(String::from);
                read_usage(&m["usage"], &mut st.usage);
                b.usage(st.usage).await;
            }
            "content_block_start" => {
                let cb = &v["content_block"];
                match cb["type"].as_str().unwrap_or_default() {
                    "text" => b.start(index, BlockKind::Text, "", "").await,
                    "thinking" => b.start(index, BlockKind::Thinking, "", "").await,
                    "redacted_thinking" => {
                        b.start(index, BlockKind::Thinking, "", "").await;
                        b.set_replay(index, json!({ "redacted": cb["data"] }));
                    }
                    "tool_use" => {
                        let id = cb["id"].as_str().unwrap_or_default();
                        let name = cb["name"].as_str().unwrap_or_default();
                        b.start(index, BlockKind::ToolCall, id, name).await;
                    }
                    _ => b.start_native(index, cb.clone()), // server tools etc., replayed verbatim
                }
            }
            "content_block_delta" => {
                let d = &v["delta"];
                match d["type"].as_str().unwrap_or_default() {
                    "text_delta" => b.delta(index, d["text"].as_str().unwrap_or_default()).await,
                    "thinking_delta" => b.delta(index, d["thinking"].as_str().unwrap_or_default()).await,
                    "input_json_delta" => b.delta(index, d["partial_json"].as_str().unwrap_or_default()).await,
                    "signature_delta" => b.set_replay(index, json!({ "signature": d["signature"] })),
                    _ => {}
                }
            }
            "content_block_stop" => b.end(index).await,
            "message_delta" => {
                read_usage(&v["usage"], &mut st.usage);
                b.usage(st.usage).await;
                st.stop = Some(match v["delta"]["stop_reason"].as_str() {
                    Some("tool_use") => StopReason::ToolUse,
                    Some("max_tokens") | Some("model_context_window_exceeded") => StopReason::Length,
                    _ => StopReason::Stop, // end_turn, stop_sequence, pause_turn, refusal
                });
            }
            "message_stop" => return Ok(Some(st.stop.clone().unwrap_or(StopReason::Stop))),
            "error" => {
                let e = &v["error"];
                let msg = e["message"].as_str().unwrap_or(data).to_string();
                return Err((error_kind(e["type"].as_str().unwrap_or_default(), &msg), msg));
            }
            _ => {} // ping
        }
        Ok(None)
    }
}

fn read_usage(u: &Value, usage: &mut Usage) {
    let get = |k: &str| u[k].as_u64();
    if let Some(n) = get("input_tokens") {
        usage.input = n;
    }
    if let Some(n) = get("output_tokens") {
        usage.output = n;
    }
    if let Some(n) = get("cache_read_input_tokens") {
        usage.cache_read = n;
    }
    if let Some(n) = get("cache_creation_input_tokens") {
        usage.cache_write = n;
    }
}

/// Map an HTTP error to a kind. String matching is allowed here, inside the adapter only (L2.19).
fn classify(status: u16, headers: &reqwest::header::HeaderMap, body: &str) -> Failure {
    let v: Value = serde_json::from_str(body).unwrap_or_default();
    let etype = v["error"]["type"].as_str().unwrap_or_default();
    let msg = v["error"]["message"]
        .as_str()
        .map(String::from)
        .unwrap_or_else(|| format!("HTTP {status}: {body}"));
    let kind = match status {
        401 | 403 => ErrorKind::Auth,
        429 => ErrorKind::RateLimit {
            retry_after_ms: retry::retry_after(headers).map(|d| d.as_millis() as u64),
        },
        408 | 409 | 500.. => ErrorKind::Transient,
        _ => error_kind(etype, &msg),
    };
    (kind, msg)
}

fn error_kind(etype: &str, msg: &str) -> ErrorKind {
    let lower = msg.to_ascii_lowercase();
    match etype {
        "overloaded_error" | "api_error" => ErrorKind::Transient,
        "rate_limit_error" => ErrorKind::RateLimit { retry_after_ms: None },
        "authentication_error" | "permission_error" => ErrorKind::Auth,
        "invalid_request_error" if lower.contains("prompt is too long") || lower.contains("context window") => {
            ErrorKind::Overflow
        }
        "invalid_request_error" if lower.contains("credit balance") => ErrorKind::QuotaExhausted,
        "invalid_request_error" | "not_found_error" | "request_too_large" => ErrorKind::InvalidRequest,
        _ => ErrorKind::Other,
    }
}

// ---------- request body ----------

/// How a Claude model takes a thinking level (L2.16).
// shortcut: decided from the model id (pi's generator rules) until the model catalog carries it (M5, L2.10).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThinkingStyle {
    /// Claude 3.x before 3.7: no thinking.
    None,
    /// `thinking: enabled` with a token budget (3.7, 4.0–4.5).
    Budget,
    /// `thinking: adaptive` plus a top-level effort.
    Adaptive,
    /// Adaptive, with the effort sent as system messages inside the conversation,
    /// so a level change keeps the cache.
    Managed,
}

/// Models that take `system` messages inside the conversation (pi's verified list).
fn mid_convo_system(model: &str) -> bool {
    let id = base_id(model).replace('.', "-");
    [
        "claude-opus-4-8",
        "claude-opus-5",
        "claude-opus-5-5",
        "claude-sonnet-5-5",
        "claude-haiku-5-5",
    ]
    .contains(&id.as_str())
        || ["claude-fable-5", "claude-mythos-5"].iter().any(|p| id.starts_with(p))
}

fn base_id(model: &str) -> String {
    let id = model.to_lowercase();
    // Drop a dated snapshot suffix (`-20260723`).
    match id.rsplit_once('-') {
        Some((head, date)) if date.len() == 8 && date.bytes().all(|b| b.is_ascii_digit()) => head.to_string(),
        _ => id,
    }
}

pub fn thinking_style(model: &str) -> ThinkingStyle {
    let id = base_id(model).replace('.', "-");
    let managed = [
        "claude-opus-5",
        "claude-opus-5-5",
        "claude-sonnet-5-5",
        "claude-haiku-5-5",
        "claude-fable-5-1",
        "claude-mythos-5-1",
    ];
    let adaptive = [
        "opus-4-6",
        "opus-4-7",
        "opus-4-8",
        "opus-5",
        "sonnet-4-6",
        "sonnet-5",
        "haiku-5",
        "fable-5",
        "mythos-5",
    ];
    if managed.contains(&id.as_str()) {
        ThinkingStyle::Managed
    } else if adaptive.iter().any(|a| id.contains(a)) {
        ThinkingStyle::Adaptive
    } else if id.starts_with("claude-3-") && !id.starts_with("claude-3-7") || id.starts_with("claude-2") {
        ThinkingStyle::None
    } else {
        ThinkingStyle::Budget
    }
}

/// The nearest level `model` supports.
pub fn clamp_thinking(model: &str, level: ThinkingLevel) -> ThinkingLevel {
    use ThinkingLevel::*;
    let id = base_id(model).replace('.', "-");
    let style = thinking_style(model);
    match (style, level) {
        (ThinkingStyle::None, _) => Off,
        (ThinkingStyle::Budget, Xhigh | Max) => High,
        (ThinkingStyle::Budget, l) => l,
        (_, Off) => {
            // Opus 5.5, Sonnet 5.5, Fable and Mythos reject `thinking: disabled`.
            let can_off = match style {
                ThinkingStyle::Managed => id == "claude-opus-5" || id == "claude-haiku-5-5",
                _ => !id.contains("fable-5") && !id.contains("mythos-5"),
            };
            if can_off { Off } else { Low }
        }
        (_, Minimal) => Low,
        (_, Xhigh) => {
            let has = ["opus-4-7", "opus-4-8", "opus-5", "sonnet-5", "haiku-5", "fable-5"];
            if has.iter().any(|h| id.contains(h)) {
                Xhigh
            } else {
                High
            }
        }
        (_, l) => l,
    }
}

fn effort_name(l: ThinkingLevel) -> &'static str {
    match l {
        ThinkingLevel::Off | ThinkingLevel::Minimal => "low",
        l => l.as_str(),
    }
}

/// The effort sent inside the conversation, when `model` takes it that way.
pub fn in_conversation_effort(model: &str, level: Option<ThinkingLevel>) -> Option<String> {
    let l = clamp_thinking(model, level?);
    (thinking_style(model) == ThinkingStyle::Managed && l != ThinkingLevel::Off).then(|| effort_name(l).to_string())
}

/// Build the JSON body. Pure and deterministic (L1.7 rule 3): same request, same bytes.
pub fn build_body(req: &Request, oauth: Option<&OAuthProfile>, ns: &str) -> Value {
    let history = transform::for_target(
        &req.messages,
        &Target {
            provider: ns,
            model: &req.model,
            images: req.images,
            normalize_id: Some(transform::safe_id),
        },
    );
    let cache = match req.cache {
        CacheRetention::Short => json!({ "type": "ephemeral" }),
        CacheRetention::Long => json!({ "type": "ephemeral", "ttl": "1h" }),
    };
    let mut system: Vec<Value> = Vec::new();
    if let Some(p) = oauth {
        system.push(json!({ "type": "text", "text": format!("x-anthropic-billing-header: {}", p.billing_header) }));
        system.push(json!({ "type": "text", "text": p.identity }));
    }
    // The leading System messages are the prompt; later ones are deltas (L4.7) and
    // stay where they are, so the cached prefix never changes.
    for m in history.iter().take_while(|m| matches!(m, Message::System { .. })) {
        if let Message::System { text } = m
            && !text.is_empty()
        {
            system.push(json!({ "type": "text", "text": text }));
        }
    }
    let native = (ns == "claude" || ns == "anthropic") && mid_convo_system(&req.model);
    let (mut messages, levels) = convert_messages(&history, ns, native);
    let tools: Vec<Value> = req
        .tools
        .iter()
        .map(|t| json!({ "name": t.name, "description": t.description, "input_schema": t.schema }))
        .collect();
    place_cache_markers(&mut system, &mut messages, &cache);
    let level = req.thinking.map(|l| clamp_thinking(&req.model, l));
    let style = thinking_style(&req.model);
    // Mid-conversation effort (after the cache markers, so they stay on real messages):
    // each reply's effort goes before it and the active one goes last, so changing the
    // level only appends and the cached prefix survives.
    if let Some(effort) = in_conversation_effort(&req.model, req.thinking) {
        let marker = |e: &str| json!({ "role": "system", "content": [], "output_config": { "effort": e } });
        let mut with = Vec::with_capacity(messages.len() * 2);
        for (m, l) in messages.into_iter().zip(levels) {
            if let Some(l) = l {
                with.push(marker(&l));
            }
            with.push(m);
        }
        with.push(marker(&effort));
        messages = with;
    }
    let mut tools = tools;
    if system.is_empty()
        && let Some(last) = tools.last_mut()
    {
        last["cache_control"] = cache.clone();
    }

    let mut body = Map::new();
    body.insert("model".into(), json!(req.model));
    let mut max_tokens = req.max_tokens;
    match (style, level) {
        (_, None) | (ThinkingStyle::None, _) | (ThinkingStyle::Budget, Some(ThinkingLevel::Off)) => {}
        (_, Some(ThinkingLevel::Off)) => {
            body.insert("thinking".into(), json!({ "type": "disabled" }));
        }
        (ThinkingStyle::Budget, Some(l)) => {
            let budget = match l {
                ThinkingLevel::Minimal => 1024,
                ThinkingLevel::Low => 2048,
                ThinkingLevel::Medium => 8192,
                _ => 16_384,
            };
            // The budget is part of max_tokens; keep the answer's room (pi's rule).
            max_tokens += budget;
            body.insert("thinking".into(), json!({ "type": "enabled", "budget_tokens": budget }));
        }
        (ThinkingStyle::Adaptive, Some(l)) => {
            body.insert(
                "thinking".into(),
                json!({ "type": "adaptive", "display": "summarized" }),
            );
            body.insert("output_config".into(), json!({ "effort": effort_name(l) }));
        }
        (ThinkingStyle::Managed, Some(_)) => {
            // The real effort travels in the messages; this top-level value stays fixed.
            body.insert(
                "thinking".into(),
                json!({ "type": "adaptive", "display": "summarized",
                        "block_binding": { "prefix_mismatch_behavior": "drop_block" } }),
            );
            body.insert("output_config".into(), json!({ "effort": "high" }));
        }
    }
    body.insert("max_tokens".into(), json!(max_tokens));
    if !system.is_empty() {
        body.insert("system".into(), Value::Array(system));
    }
    body.insert("messages".into(), Value::Array(messages));
    if !tools.is_empty() {
        body.insert("tools".into(), Value::Array(tools));
    }
    body.insert("stream".into(), json!(true));
    Value::Object(body)
}

/// Prefix + sliding pair (L2.17): end of system (covers tools+system), the
/// last message, and the last message of the previous turn. At most 3 of 4.
fn place_cache_markers(system: &mut [Value], messages: &mut [Value], cache: &Value) {
    if let Some(last) = system.last_mut() {
        last["cache_control"] = cache.clone();
    }
    // The last real (non-system) message, and the previous turn's last reply.
    let last = messages.iter().rposition(|m| m["role"] != "system");
    let prev_turn_end = last.and_then(|l| messages[..l].iter().rposition(|m| m["role"] == "assistant"));
    for i in [prev_turn_end, last].into_iter().flatten() {
        let blocks = messages[i]["content"].as_array_mut();
        // Thinking blocks can't carry cache_control; mark the last block that can.
        if let Some(b) = blocks.and_then(|bs| {
            bs.iter_mut()
                .rev()
                .find(|b| !b["type"].as_str().unwrap_or_default().contains("thinking"))
        }) {
            b["cache_control"] = cache.clone();
        }
    }
}

/// Wire messages, plus the recorded effort of each assistant one (for mid-conversation effort).
fn convert_messages(messages: &[Message], ns: &str, native_system: bool) -> (Vec<Value>, Vec<Option<String>>) {
    let mut out: Vec<Value> = Vec::new();
    let mut levels: Vec<Option<String>> = Vec::new();
    let mut push = |role: &str, blocks: Vec<Value>, level: Option<&String>| {
        if blocks.is_empty() {
            return;
        }
        // Consecutive same-role turns merge (tool results + following user text).
        if let Some(last) = out.last_mut()
            && last["role"] == role
            && let Some(arr) = last["content"].as_array_mut()
        {
            arr.extend(blocks);
            return;
        }
        out.push(json!({ "role": role, "content": blocks }));
        levels.push(level.cloned());
    };
    let lead = messages
        .iter()
        .take_while(|m| matches!(m, Message::System { .. }))
        .count();
    let reminder = |t: &str| json!({ "type": "text", "text": format!("<system-reminder>\n{t}\n</system-reminder>") });
    // A native system message must follow a user message: deltas wait for the next one.
    let mut held: Vec<Value> = Vec::new();
    for m in &messages[lead..] {
        match m {
            Message::System { text } if text.is_empty() => {}
            Message::System { text } if native_system => held.push(json!({ "type": "text", "text": text })),
            // Older models: the delta rides in the conversation as a reminder.
            Message::System { text } => push("user", vec![reminder(text)], None),
            Message::Custom { .. } => {}
            Message::User { content } => {
                push("user", content.iter().filter_map(user_block).collect(), None);
                if !held.is_empty() {
                    push("system", std::mem::take(&mut held), None);
                }
            }
            Message::Assistant(a) => {
                let same = a.provider == ns;
                push(
                    "assistant",
                    a.content.iter().filter_map(|b| assistant_block(b, ns, same)).collect(),
                    a.thinking_level.as_ref().filter(|_| same),
                )
            }
            Message::ToolResult {
                call_id,
                content,
                is_error,
                ..
            } => {
                let mut inner: Vec<Value> = content.iter().filter_map(user_block).collect();
                if inner.is_empty() {
                    inner.push(json!({ "type": "text", "text": "(no output)" }));
                }
                let mut r = json!({ "type": "tool_result", "tool_use_id": call_id, "content": inner });
                if *is_error {
                    r["is_error"] = json!(true);
                }
                push("user", vec![r], None);
            }
        }
    }
    // Nothing followed: keep the text as a reminder rather than lose it.
    let left: Vec<Value> = held
        .iter()
        .map(|b| reminder(b["text"].as_str().unwrap_or_default()))
        .collect();
    push("user", left, None);
    (out, levels)
}

fn user_block(b: &Block) -> Option<Value> {
    match b {
        Block::Text { text, .. } if !text.is_empty() => Some(json!({ "type": "text", "text": text })),
        Block::Image { mime_type, data } => {
            Some(json!({ "type": "image", "source": { "type": "base64", "media_type": mime_type, "data": data } }))
        }
        _ => None,
    }
}

fn assistant_block(b: &Block, ns: &str, same_provider: bool) -> Option<Value> {
    let replay_data =
        |r: &Option<novacode_types::Replay>| r.as_ref().filter(|r| r.provider == ns).map(|r| r.data.clone());
    match b {
        Block::Text { text, .. } if !text.is_empty() => Some(json!({ "type": "text", "text": text })),
        Block::Thinking { text, replay } => {
            // Thinking is replayed only with its signature from Anthropic; otherwise dropped (L2.15).
            let data = replay_data(replay)?;
            if let Some(r) = data.get("redacted") {
                return Some(json!({ "type": "redacted_thinking", "data": r }));
            }
            Some(json!({ "type": "thinking", "thinking": text, "signature": data.get("signature")? }))
        }
        Block::ToolCall(c) => Some(json!({ "type": "tool_use", "id": c.id, "name": c.name, "input": c.arguments })),
        Block::ProviderNative { provider, item } if same_provider && provider == ns => Some(item.clone()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter::collect;
    use novacode_types::{AssistantMessage, Replay, ToolCall, ToolDef};

    fn req(messages: Vec<Message>) -> Request {
        Request {
            model: "claude-x".into(),
            messages,
            tools: vec![],
            max_tokens: 100,
            cache: CacheRetention::Short,
            thinking: None,
            images: true,
            session: None,
        }
    }

    #[test]
    fn later_system_messages_stay_in_place() {
        let sys = |t: &str| Message::System { text: t.into() };
        let history = vec![sys("prompt"), Message::user("hi"), sys("delta"), Message::user("next")];
        let mut r = req(history);
        r.model = "claude-opus-5-5".into();
        let b = build_body(&r, None, "claude");
        assert_eq!(
            b["system"].as_array().unwrap().len(),
            1,
            "only the leading prompt is top-level"
        );
        let m = b["messages"].as_array().unwrap();
        // Held until a user message precedes it (the API's rule), then marked nowhere.
        let roles: Vec<&str> = m.iter().map(|x| x["role"].as_str().unwrap()).collect();
        assert_eq!(roles, ["user", "system"]);
        assert_eq!(m[1]["content"][0]["text"], "delta");
        assert!(
            m[1]["content"][0].get("cache_control").is_none(),
            "markers go on real messages"
        );
        assert!(m[0]["content"][1].get("cache_control").is_some());
        // Older model, or another vendor on this wire: a reminder inside the user turn.
        r.model = "claude-sonnet-4-5".into();
        let b = build_body(&r, None, "claude");
        let m = b["messages"].as_array().unwrap();
        assert_eq!(m.len(), 1, "user turns merge");
        assert_eq!(
            m[0]["content"][1]["text"],
            "<system-reminder>\ndelta\n</system-reminder>"
        );
        assert!(
            !build_body(&r, None, "minimax")
                .to_string()
                .contains("\"role\":\"system\"")
        );
    }

    fn assistant(content: Vec<Block>) -> Message {
        Message::Assistant(AssistantMessage {
            content,
            stop: StopReason::ToolUse,
            usage: Usage::default(),
            provider: PROVIDER.into(),
            model: "claude-x".into(),
            response_id: None,
            error_message: None,
            thinking_level: None,
        })
    }

    #[test]
    fn thinking_levels_per_model() {
        use ThinkingLevel::*;
        assert_eq!(thinking_style("claude-opus-5-5"), ThinkingStyle::Managed);
        assert_eq!(thinking_style("claude-opus-4-8-20260101"), ThinkingStyle::Adaptive);
        assert_eq!(thinking_style("claude-sonnet-4-5"), ThinkingStyle::Budget);
        assert_eq!(thinking_style("claude-3-5-haiku"), ThinkingStyle::None);
        assert_eq!(clamp_thinking("claude-opus-5-5", Off), Low);
        assert_eq!(clamp_thinking("claude-haiku-5-5", Off), Off);
        assert_eq!(clamp_thinking("claude-opus-4-6", Xhigh), High);
        assert_eq!(clamp_thinking("claude-sonnet-4-5", Max), High);

        let mut r = req(vec![Message::user("a")]);
        r.model = "claude-sonnet-4-5".into();
        r.thinking = Some(Medium);
        let b = build_body(&r, None, PROVIDER);
        assert_eq!(b["thinking"], json!({ "type": "enabled", "budget_tokens": 8192 }));
        assert_eq!(b["max_tokens"], json!(100 + 8192));
        r.model = "claude-opus-4-8".into();
        let b = build_body(&r, None, PROVIDER);
        assert_eq!(b["output_config"], json!({ "effort": "medium" }));
    }

    #[test]
    fn mid_conversation_effort_only_appends() {
        let mut first = req(vec![Message::user("a")]);
        first.model = "claude-opus-5-5".into();
        first.thinking = Some(ThinkingLevel::High);
        let b1 = build_body(&first, None, PROVIDER);
        let m1 = b1["messages"].as_array().unwrap();
        assert_eq!(m1.last().unwrap()["output_config"], json!({ "effort": "high" }));

        // The reply records its effort; the next turn switches to low.
        let mut reply = AssistantMessage {
            content: vec![Block::text("b")],
            stop: StopReason::Stop,
            usage: Usage::default(),
            provider: PROVIDER.into(),
            model: "claude-opus-5-5".into(),
            response_id: None,
            error_message: None,
            thinking_level: None,
        };
        reply.thinking_level = in_conversation_effort(&first.model, first.thinking);
        let mut second = first.clone();
        second.messages.extend([Message::Assistant(reply), Message::user("c")]);
        second.thinking = Some(ThinkingLevel::Low);
        let b2 = build_body(&second, None, PROVIDER);
        let m2 = b2["messages"].as_array().unwrap();
        let roles: Vec<&str> = m2.iter().map(|m| m["role"].as_str().unwrap()).collect();
        assert_eq!(roles, ["user", "system", "assistant", "user", "system"]);
        assert_eq!(m2[1], m1[1], "the old effort marker is unchanged");
        assert_eq!(m2[4]["output_config"], json!({ "effort": "low" }));
        assert_eq!(b2["output_config"], b1["output_config"], "top level never changes");
        assert_eq!(b2["thinking"], b1["thinking"]);
    }

    #[test]
    fn body_shape_markers_and_replay() {
        let call = ToolCall {
            id: "t1".into(),
            name: "read".into(),
            arguments: json!({"p": 1}),
            arguments_error: None,
            replay: None,
        };
        let thinking = Block::Thinking {
            text: "hmm".into(),
            replay: Some(Replay {
                provider: PROVIDER.into(),
                data: json!({"signature": "sig"}),
            }),
        };
        let foreign = Block::Thinking {
            text: "x".into(),
            replay: Some(Replay {
                provider: "openai".into(),
                data: json!({}),
            }),
        };
        let mut r = req(vec![
            Message::System {
                text: "be brief".into(),
            },
            Message::user("hi"),
            assistant(vec![thinking, foreign, Block::ToolCall(call)]),
            Message::ToolResult {
                call_id: "t1".into(),
                tool_name: "read".into(),
                content: vec![Block::text("ok")],
                is_error: false,
                details: Some(json!({"ui": 1})),
            },
            Message::user("next"),
            Message::Custom {
                kind: "note".into(),
                data: json!(1),
            },
        ]);
        r.tools = vec![ToolDef {
            name: "read".into(),
            description: "Read".into(),
            schema: json!({"type": "object"}),
        }];
        let body = build_body(&r, None, PROVIDER);
        let want = json!({
            "model": "claude-x", "max_tokens": 100,
            "system": [{"type": "text", "text": "be brief", "cache_control": {"type": "ephemeral"}}],
            "messages": [
                {"role": "user", "content": [{"type": "text", "text": "hi"}]},
                {"role": "assistant", "content": [
                    {"type": "thinking", "thinking": "hmm", "signature": "sig"},
                    {"type": "tool_use", "id": "t1", "name": "read", "input": {"p": 1}, "cache_control": {"type": "ephemeral"}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "t1", "content": [{"type": "text", "text": "ok"}]},
                    {"type": "text", "text": "next", "cache_control": {"type": "ephemeral"}}
                ]}
            ],
            "tools": [{"name": "read", "description": "Read", "input_schema": {"type": "object"}}],
            "stream": true
        });
        assert_eq!(body, want);
        assert_eq!(
            serde_json::to_vec(&body).unwrap(),
            serde_json::to_vec(&build_body(&r, None, PROVIDER)).unwrap()
        );
    }

    #[test]
    fn oauth_system_blocks_and_long_ttl() {
        let mut r = req(vec![Message::user("hi")]);
        r.cache = CacheRetention::Long;
        let body = build_body(&r, Some(&OAuthProfile::default()), PROVIDER);
        let sys = body["system"].as_array().unwrap();
        assert!(
            sys[0]["text"]
                .as_str()
                .unwrap()
                .starts_with("x-anthropic-billing-header: cc_version=")
        );
        assert_eq!(sys[1]["cache_control"], json!({"type": "ephemeral", "ttl": "1h"}));
    }

    #[test]
    fn error_classification() {
        let h = reqwest::header::HeaderMap::new();
        let body = |t: &str, m: &str| json!({"type": "error", "error": {"type": t, "message": m}}).to_string();
        assert_eq!(
            classify(
                400,
                &h,
                &body("invalid_request_error", "prompt is too long: 210000 tokens > 200000")
            )
            .0,
            ErrorKind::Overflow
        );
        assert_eq!(
            classify(400, &h, &body("invalid_request_error", "bad field")).0,
            ErrorKind::InvalidRequest
        );
        assert_eq!(classify(401, &h, "").0, ErrorKind::Auth);
        assert_eq!(
            classify(529, &h, &body("overloaded_error", "Overloaded")).0,
            ErrorKind::Transient
        );
        assert_eq!(classify(429, &h, "").0, ErrorKind::RateLimit { retry_after_ms: None });
    }

    /// One-shot HTTP server replaying canned responses, one per connection.
    async fn serve(responses: Vec<String>) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move {
            for r in responses {
                let (mut s, _) = l.accept().await.unwrap();
                let mut buf = vec![0u8; 65536];
                let _ = s.read(&mut buf).await;
                s.write_all(r.as_bytes()).await.unwrap();
                s.shutdown().await.unwrap();
            }
        });
        format!("http://{addr}")
    }

    fn http(status: &str, headers: &str, body: &str) -> String {
        format!(
            "HTTP/1.1 {status}\r\ncontent-length: {}\r\nconnection: close\r\n{headers}\r\n{body}",
            body.len()
        )
    }

    const SSE: &str = "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"usage\":{\"input_tokens\":5,\"cache_read_input_tokens\":100,\"cache_creation_input_tokens\":7,\"output_tokens\":1}}}\n\n\
event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"\"}}\n\n\
event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"plan\"}}\n\n\
event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"signature_delta\",\"signature\":\"S\"}}\n\n\
event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n\
event: ping\ndata: {\"type\":\"ping\"}\n\n\
event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"tool_use\",\"id\":\"tu1\",\"name\":\"read\",\"input\":{}}}\n\n\
event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"path\\\":\"}}\n\n\
event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"\\\"a.rs\\\"}\"}}\n\n\
event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":1}\n\n\
event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"},\"usage\":{\"output_tokens\":30}}\n\n\
event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";

    #[tokio::test]
    async fn streams_after_retrying_overload() {
        let overloaded = http(
            "529 Overloaded",
            "retry-after-ms: 10\r\n",
            r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#,
        );
        let ok = http("200 OK", "content-type: text/event-stream\r\n", SSE);
        let url = serve(vec![overloaded, ok]).await;
        let a = Anthropic::new(Auth::ApiKey("k".into()), None).with_base_url(url);
        let m = collect(a.stream(req(vec![Message::user("hi")]), CancellationToken::new())).await;
        assert_eq!(m.stop, StopReason::ToolUse, "{:?}", m.error_message);
        assert_eq!(
            m.usage,
            Usage {
                input: 5,
                output: 30,
                cache_read: 100,
                cache_write: 7,
                reasoning: None
            }
        );
        assert_eq!(m.response_id.as_deref(), Some("msg_1"));
        assert_eq!(
            m.content[0],
            Block::Thinking {
                text: "plan".into(),
                replay: Some(Replay {
                    provider: PROVIDER.into(),
                    data: json!({"signature": "S"})
                })
            }
        );
        let call = m.tool_calls().next().unwrap();
        assert_eq!((call.id.as_str(), &call.arguments), ("tu1", &json!({"path": "a.rs"})));
    }

    #[tokio::test]
    async fn overflow_is_not_retried_and_cut_stream_is_transient() {
        let overflow = http(
            "400 Bad Request",
            "",
            r#"{"type":"error","error":{"type":"invalid_request_error","message":"prompt is too long"}}"#,
        );
        let url = serve(vec![overflow]).await;
        let a = Anthropic::new(Auth::ApiKey("k".into()), None).with_base_url(url);
        let m = collect(a.stream(req(vec![Message::user("hi")]), CancellationToken::new())).await;
        assert_eq!(
            m.stop,
            StopReason::Error {
                kind: ErrorKind::Overflow
            }
        );

        let cut = &SSE[..SSE.find("event: content_block_stop").unwrap()];
        let url = serve(vec![http("200 OK", "content-type: text/event-stream\r\n", cut)]).await;
        let a = Anthropic::new(Auth::ApiKey("k".into()), None).with_base_url(url);
        let m = collect(a.stream(req(vec![Message::user("hi")]), CancellationToken::new())).await;
        assert_eq!(
            m.stop,
            StopReason::Error {
                kind: ErrorKind::Transient
            }
        );
    }
}
