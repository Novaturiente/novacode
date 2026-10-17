//! Anthropic Messages adapter (L2.5) with API-key and Claude subscription
//! (OAuth) profiles (L2.1), cache markers (L2.17), retry before content (L2.18),
//! overflow detection (L2.19) and redacted tracing (L2.24).

use crate::adapter::{Adapter, Builder, CacheRetention, EVENT_BUFFER, Request};
use crate::retry;
use crate::sse::SseParser;
use crate::trace::Trace;
use futures_util::StreamExt;
use novacode_types::{Block, BlockKind, ErrorKind, Message, StopReason, StreamEvent, Usage};
use serde_json::{Map, Value, json};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

pub const PROVIDER: &str = "anthropic";
const API_VERSION: &str = "2023-06-01";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
/// Time allowed between any two chunks (also covers time to first byte).
const IDLE_TIMEOUT: Duration = Duration::from_secs(120);

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
}

pub struct Anthropic {
    http: reqwest::Client,
    base_url: String,
    auth: Auth,
    trace: Option<Trace>,
}

impl Anthropic {
    pub fn new(auth: Auth, trace: Option<Trace>) -> Self {
        let http = reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .build()
            .unwrap_or_default();
        Self {
            http,
            base_url: "https://api.anthropic.com".into(),
            auth,
            trace,
        }
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
            .authed(self.http.get(url))
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

    fn authed(&self, r: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        let r = r.header("anthropic-version", API_VERSION);
        match &self.auth {
            Auth::ApiKey(k) => r.header("x-api-key", k),
            Auth::OAuth { access_token, profile } => r
                .bearer_auth(access_token)
                .header("anthropic-beta", &profile.betas)
                .header("user-agent", &profile.user_agent)
                .header("x-app", "cli"),
        }
    }

    fn http_request(&self, body: &[u8]) -> reqwest::RequestBuilder {
        let oauth = matches!(self.auth, Auth::OAuth { .. });
        let url = format!("{}/v1/messages{}", self.base_url, if oauth { "?beta=true" } else { "" });
        let r = self
            .http
            .post(url)
            .header("content-type", "application/json")
            .header("accept", "text/event-stream");
        self.authed(r).body(body.to_vec())
    }
}

impl Adapter for Anthropic {
    fn stream(&self, req: Request, cancel: CancellationToken) -> mpsc::Receiver<StreamEvent> {
        let (tx, rx) = mpsc::channel(EVENT_BUFFER);
        let oauth_profile = match &self.auth {
            Auth::OAuth { profile, .. } => Some(profile.clone()),
            Auth::ApiKey(_) => None,
        };
        let body = serde_json::to_vec(&build_body(&req, oauth_profile.as_ref())).unwrap_or_default();
        let request = self.http_request(&body);
        let trace = self.trace.clone();
        tokio::spawn(async move {
            let mut b = Builder::new(tx, PROVIDER, &req.model);
            if let Some(t) = &trace {
                t.request(&request, &body);
            }
            tokio::select! {
                _ = cancel.cancelled() => b.fail(ErrorKind::Aborted, "aborted").await,
                r = run(request, &mut b, trace.as_ref()) => match r {
                    Ok(stop) => b.finish(stop).await,
                    Err((kind, msg)) => b.fail(kind, msg).await,
                },
            }
        });
        rx
    }
}

type Failure = (ErrorKind, String);

/// Send with retries before any content, then stream.
async fn run(request: reqwest::RequestBuilder, b: &mut Builder, trace: Option<&Trace>) -> Result<StopReason, Failure> {
    let mut attempt = 0;
    loop {
        let Some(req) = request.try_clone() else {
            return Err((ErrorKind::Other, "request not cloneable".into()));
        };
        let failure = match req.send().await {
            Err(e) => Some((None, None, (ErrorKind::Transient, format!("network error: {e}")))),
            Ok(resp) if !resp.status().is_success() => {
                let status = resp.status().as_u16();
                let headers = resp.headers().clone();
                let text = resp.text().await.unwrap_or_default();
                if let Some(t) = trace {
                    t.response_error(status, &text);
                }
                Some((Some(status), Some(headers.clone()), classify(status, &headers, &text)))
            }
            Ok(resp) => match stream_body(resp, b, trace).await {
                Ok(stop) => return Ok(stop),
                // Mid-stream failures are not retried here; the loop decides (L4.6).
                Err(f) if b.started => return Err(f),
                Err(f) => Some((None, None, f)),
            },
        };
        let Some((status, headers, f)) = failure else {
            unreachable!()
        };
        if !retry::should_retry(status, headers.as_ref()) || !f.0.is_retryable() {
            return Err(f);
        }
        let server = headers.as_ref().and_then(retry::retry_after);
        let Some(d) = retry::delay(attempt, server) else {
            return Err(f);
        };
        tracing::warn!(attempt, ?d, error = %f.1, "anthropic: retrying");
        b.rollback().await;
        tokio::time::sleep(d).await;
        attempt += 1;
    }
}

async fn stream_body(resp: reqwest::Response, b: &mut Builder, trace: Option<&Trace>) -> Result<StopReason, Failure> {
    let mut body = resp.bytes_stream();
    let mut sse = SseParser::default();
    let mut st = State::default();
    loop {
        let chunk = match tokio::time::timeout(IDLE_TIMEOUT, body.next()).await {
            Err(_) => return Err((ErrorKind::Transient, "stream idle timeout".into())),
            Ok(None) => break,
            Ok(Some(Err(e))) => return Err((ErrorKind::Transient, format!("stream error: {e}"))),
            Ok(Some(Ok(c))) => c,
        };
        for ev in sse.push(&chunk) {
            if let Some(t) = trace {
                t.sse(&ev);
            }
            if let Some(stop) = on_event(&ev.event, &ev.data, b, &mut st).await? {
                return Ok(stop);
            }
        }
    }
    if let Some(ev) = sse.finish()
        && let Some(stop) = on_event(&ev.event, &ev.data, b, &mut st).await?
    {
        return Ok(stop);
    }
    match st.stop {
        Some(stop) => Ok(stop),
        None => Err((ErrorKind::Transient, "stream ended before message_stop".into())),
    }
}

#[derive(Default)]
struct State {
    stop: Option<StopReason>,
    usage: Usage,
}

/// Handle one SSE event; `Some(stop)` ends the stream.
async fn on_event(event: &str, data: &str, b: &mut Builder, st: &mut State) -> Result<Option<StopReason>, Failure> {
    if data.is_empty() {
        return Ok(None);
    }
    let v: Value = serde_json::from_str(data).map_err(|e| (ErrorKind::Transient, format!("bad event json: {e}")))?;
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

/// Build the JSON body. Pure and deterministic (L1.7 rule 3): same request, same bytes.
pub fn build_body(req: &Request, oauth: Option<&OAuthProfile>) -> Value {
    let cache = match req.cache {
        CacheRetention::Short => json!({ "type": "ephemeral" }),
        CacheRetention::Long => json!({ "type": "ephemeral", "ttl": "1h" }),
    };
    let mut system: Vec<Value> = Vec::new();
    if let Some(p) = oauth {
        system.push(json!({ "type": "text", "text": format!("x-anthropic-billing-header: {}", p.billing_header) }));
        system.push(json!({ "type": "text", "text": p.identity }));
    }
    // Anthropic has no mid-conversation system role: merge every System message, in order (L1.1).
    for m in &req.messages {
        if let Message::System { text } = m
            && !text.is_empty()
        {
            system.push(json!({ "type": "text", "text": text }));
        }
    }
    let mut messages = convert_messages(&req.messages);
    let tools: Vec<Value> = req
        .tools
        .iter()
        .map(|t| json!({ "name": t.name, "description": t.description, "input_schema": t.schema }))
        .collect();
    place_cache_markers(&mut system, &mut messages, &cache);
    let mut tools = tools;
    if system.is_empty()
        && let Some(last) = tools.last_mut()
    {
        last["cache_control"] = cache.clone();
    }

    let mut body = Map::new();
    body.insert("model".into(), json!(req.model));
    body.insert("max_tokens".into(), json!(req.max_tokens));
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
    let n = messages.len();
    let prev_turn_end = messages[..n.saturating_sub(1)]
        .iter()
        .rposition(|m| m["role"] == "assistant");
    for i in [prev_turn_end, n.checked_sub(1)].into_iter().flatten() {
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

fn convert_messages(messages: &[Message]) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::new();
    let mut push = |role: &str, blocks: Vec<Value>| {
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
    };
    for m in messages {
        match m {
            Message::System { .. } | Message::Custom { .. } => {}
            Message::User { content } => push("user", content.iter().filter_map(user_block).collect()),
            Message::Assistant(a) => {
                let same = a.provider == PROVIDER;
                push(
                    "assistant",
                    a.content.iter().filter_map(|b| assistant_block(b, same)).collect(),
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
                push("user", vec![r]);
            }
        }
    }
    out
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

fn assistant_block(b: &Block, same_provider: bool) -> Option<Value> {
    let replay_data =
        |r: &Option<novacode_types::Replay>| r.as_ref().filter(|r| r.provider == PROVIDER).map(|r| r.data.clone());
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
        Block::ProviderNative { provider, item } if same_provider && provider == PROVIDER => Some(item.clone()),
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
        }
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
        })
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
        let body = build_body(&r, None);
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
            serde_json::to_vec(&build_body(&r, None)).unwrap()
        );
    }

    #[test]
    fn oauth_system_blocks_and_long_ttl() {
        let mut r = req(vec![Message::user("hi")]);
        r.cache = CacheRetention::Long;
        let body = build_body(&r, Some(&OAuthProfile::default()));
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
