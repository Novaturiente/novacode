//! OpenAI Responses adapter (L2.6): the OpenAI API with a key, and the
//! ChatGPT/Codex subscription (same wire, different URL, auth and headers).
//! Reasoning is replayed as the encrypted item it arrived in (`store: false`).

use crate::adapter::{Adapter, Builder, Request};
use crate::auth::codex;
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

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Compat {
    /// The endpoint rejects `max_output_tokens` (the Codex backend).
    pub no_max_tokens: bool,
    /// Thinking `off` can't be sent as effort `none` (GitHub Copilot); the model's default is used.
    pub no_effort_none: bool,
}

pub enum Auth {
    ApiKey(String),
    /// ChatGPT subscription token; the account id comes from it.
    Codex {
        access_token: String,
        originator: String,
    },
}

pub struct OpenAiResponses {
    http: reqwest::Client,
    base_url: String,
    auth: Auth,
    compat: Compat,
    headers: BTreeMap<String, String>,
    /// Replay namespace = profile id (`openai` or `codex`).
    provider: String,
    trace: Option<Trace>,
    /// Swap the `web_search` function tool for OpenAI's hosted search (L2.23).
    native_search: bool,
}

impl OpenAiResponses {
    pub fn with_native_search(mut self, on: bool) -> Self {
        self.native_search = on;
        self
    }

    pub fn new(provider: &str, base_url: &str, auth: Auth, compat: Compat, trace: Option<Trace>) -> Self {
        Self {
            http: http::client(),
            base_url: base_url.trim_end_matches('/').into(),
            auth,
            compat,
            headers: BTreeMap::new(),
            provider: provider.into(),
            trace,
            native_search: false,
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
        r = r.header("user-agent", concat!("novacode/", env!("CARGO_PKG_VERSION")));
        match &self.auth {
            Auth::ApiKey(k) => r = r.bearer_auth(k),
            Auth::Codex {
                access_token,
                originator,
            } => {
                r = r.bearer_auth(access_token).header("originator", originator);
                if let Some(id) = codex::account_id(access_token) {
                    r = r.header("chatgpt-account-id", id);
                }
            }
        }
        for (k, v) in &self.headers {
            r = r.header(k, v);
        }
        r
    }

    async fn models(&self) -> Result<Vec<String>, String> {
        let url = match self.auth {
            // The backend hides models newer than the client; a high version lists them all.
            Auth::Codex { .. } => format!("{}/models?client_version=99.0.0", self.base_url),
            Auth::ApiKey(_) => format!("{}/models", self.base_url),
        };
        let resp = self
            .authed(self.http.get(url))
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
        let v: Value = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
        // API: {data: [{id}]}; Codex: {models: [{slug, visibility}]}.
        let mut ids: Vec<String> = v["data"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|m| m["id"].as_str().map(String::from))
            .chain(
                v["models"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter(|m| m["visibility"].as_str().is_none_or(|s| s == "list"))
                    .filter_map(|m| m["slug"].as_str().or(m["id"].as_str()).map(String::from)),
            )
            .collect();
        ids.sort();
        ids.dedup();
        Ok(ids)
    }
}

impl Adapter for OpenAiResponses {
    fn stream(&self, req: Request, cancel: CancellationToken) -> mpsc::Receiver<StreamEvent> {
        let mut body = build_body(&req, &self.compat, &self.provider);
        if self.native_search {
            crate::anthropic::swap_search_tool(&mut body, json!({ "type": "web_search" }));
        }
        let body = serde_json::to_vec(&body).unwrap_or_default();
        let mut r = self
            .authed(self.http.post(format!("{}/responses", self.base_url)))
            .header("content-type", "application/json")
            .header("accept", "text/event-stream");
        if let Auth::Codex { .. } = self.auth {
            r = r.header("OpenAI-Beta", "responses=experimental");
            if let Some(s) = &req.session {
                r = r.header("session_id", s);
            }
        }
        let request = r.body(body.clone());
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
pub fn build_body(req: &Request, c: &Compat, ns: &str) -> Value {
    let history = transform::for_target(
        &req.messages,
        &Target {
            provider: ns,
            model: &req.model,
            images: req.images,
            normalize_id: Some(transform::safe_id),
        },
    );
    // The leading system prompt is `instructions`; later system deltas are developer items (L1.1).
    let lead = history.iter().take_while(|m| matches!(m, Message::System { .. }));
    let instructions: Vec<&str> = lead
        .filter_map(|m| match m {
            Message::System { text } if !text.is_empty() => Some(text.as_str()),
            _ => None,
        })
        .collect();
    let skip = history
        .iter()
        .take_while(|m| matches!(m, Message::System { .. }))
        .count();
    let input = convert(&history[skip..], ns, &req.model);
    let mut body = Map::new();
    body.insert("model".into(), json!(req.model));
    let instructions = if instructions.is_empty() {
        "You are a helpful assistant.".to_string()
    } else {
        instructions.join("\n\n")
    };
    body.insert("instructions".into(), json!(instructions));
    body.insert("input".into(), Value::Array(input));
    if !req.tools.is_empty() {
        let tools: Vec<Value> = req
            .tools
            .iter()
            .map(|t| json!({ "type": "function", "name": t.name, "description": t.description, "parameters": t.schema, "strict": false }))
            .collect();
        body.insert("tools".into(), Value::Array(tools));
        body.insert("tool_choice".into(), json!("auto"));
        body.insert("parallel_tool_calls".into(), json!(true));
    }
    // `thinking` is only set for reasoning models (L2.16), so Off means switch reasoning off.
    match req.thinking {
        Some(ThinkingLevel::Off) if !c.no_effort_none => {
            body.insert("reasoning".into(), json!({ "effort": "none" }));
        }
        Some(ThinkingLevel::Off) | None => {}
        Some(level) => {
            body.insert(
                "reasoning".into(),
                json!({ "effort": effort_name(level), "summary": "auto" }),
            );
        }
    }
    if !c.no_max_tokens {
        body.insert("max_output_tokens".into(), json!(req.max_tokens));
    }
    body.insert("store".into(), json!(false));
    body.insert("include".into(), json!(["reasoning.encrypted_content"]));
    if let Some(s) = &req.session {
        body.insert("prompt_cache_key".into(), json!(s.chars().take(64).collect::<String>()));
    }
    body.insert("stream".into(), json!(true));
    Value::Object(body)
}

fn convert(history: &[Message], ns: &str, model: &str) -> Vec<Value> {
    let mut out = Vec::new();
    for m in history {
        match m {
            Message::System { text } if !text.is_empty() => {
                out.push(json!({ "role": "developer", "content": text }));
            }
            Message::System { .. } | Message::Custom { .. } => {}
            Message::User { content } => {
                let parts = input_parts(content);
                if !parts.is_empty() {
                    out.push(json!({ "role": "user", "content": parts }));
                }
            }
            Message::Assistant(a) => {
                let same = a.provider == ns && a.model == model;
                let replay = |r: &Option<novacode_types::Replay>| {
                    r.as_ref().filter(|r| same && r.provider == ns).map(|r| r.data.clone())
                };
                for b in &a.content {
                    match b {
                        Block::Thinking { replay: r, .. } => {
                            if let Some(item) = replay(r).and_then(|d| d.get("item").cloned()) {
                                out.push(item);
                            }
                        }
                        Block::Text { text, replay: r } if !text.is_empty() => {
                            let mut item = json!({
                                "type": "message",
                                "role": "assistant",
                                "content": [{ "type": "output_text", "text": text, "annotations": [] }],
                                "status": "completed",
                            });
                            if let Some(id) = replay(r).and_then(|d| d["id"].as_str().map(String::from)) {
                                item["id"] = json!(id);
                            }
                            out.push(item);
                        }
                        Block::ToolCall(call) => {
                            let mut item = json!({
                                "type": "function_call",
                                "call_id": call.id,
                                "name": call.name,
                                "arguments": call.arguments.to_string(),
                            });
                            // The item id pairs the call with its reasoning; only valid for the same model.
                            if let Some(id) = replay(&call.replay).and_then(|d| d["item_id"].as_str().map(String::from))
                            {
                                item["id"] = json!(id);
                            }
                            out.push(item);
                        }
                        _ => {}
                    }
                }
            }
            Message::ToolResult { call_id, content, .. } => {
                let has_image = content.iter().any(|b| matches!(b, Block::Image { .. }));
                let output = if has_image {
                    Value::Array(input_parts(content))
                } else {
                    let t: Vec<&str> = content
                        .iter()
                        .filter_map(|b| match b {
                            Block::Text { text, .. } => Some(text.as_str()),
                            _ => None,
                        })
                        .collect();
                    json!(if t.is_empty() {
                        "(no output)".to_string()
                    } else {
                        t.join("\n")
                    })
                };
                out.push(json!({ "type": "function_call_output", "call_id": call_id, "output": output }));
            }
        }
    }
    out
}

fn input_parts(content: &[Block]) -> Vec<Value> {
    content
        .iter()
        .filter_map(|b| match b {
            Block::Text { text, .. } if !text.is_empty() => Some(json!({ "type": "input_text", "text": text })),
            Block::Image { mime_type, data } => Some(json!({
                "type": "input_image",
                "detail": "auto",
                "image_url": format!("data:{mime_type};base64,{data}"),
            })),
            _ => None,
        })
        .collect()
}

// ---------- stream ----------

#[derive(Default)]
struct State {
    /// output_index → builder index, for open items.
    open: BTreeMap<u64, BlockKind>,
    /// Hosted `web_search_call` items in flight.
    searches: std::collections::BTreeSet<u64>,
    tools: usize,
}

fn usage(u: &Value) -> Usage {
    let g = |p: &str| u.pointer(p).and_then(Value::as_u64).unwrap_or(0);
    let cached = g("/input_tokens_details/cached_tokens");
    Usage {
        input: g("/input_tokens").saturating_sub(cached),
        output: g("/output_tokens"),
        cache_read: cached,
        cache_write: 0,
        reasoning: u
            .pointer("/output_tokens_details/reasoning_tokens")
            .and_then(Value::as_u64),
    }
}

impl Wire for State {
    fn classify(status: u16, headers: &reqwest::header::HeaderMap, body: &str) -> Failure {
        let (kind, msg) = http::classify_openai(status, headers, body);
        let v: Value = serde_json::from_str(body).unwrap_or_default();
        // Codex: the 5-hour or weekly window is used up (L2.14 rotates on this).
        let kind = if v["error"]["type"] == "usage_limit_reached" || v["detail"]["code"] == "usage_limit_reached" {
            ErrorKind::QuotaExhausted
        } else {
            kind
        };
        let msg = v["detail"].as_str().map(String::from).unwrap_or(msg);
        (kind, msg)
    }

    async fn at_eof(&mut self, _b: &mut Builder) -> Result<StopReason, Failure> {
        Err((ErrorKind::Transient, "stream ended before response.completed".into()))
    }

    async fn on_event(&mut self, ev: &SseEvent, b: &mut Builder) -> Result<Option<StopReason>, Failure> {
        if ev.data.trim().is_empty() || ev.data.trim() == "[DONE]" {
            return Ok(None);
        }
        let v: Value =
            serde_json::from_str(&ev.data).map_err(|e| (ErrorKind::Transient, format!("bad event json: {e}")))?;
        let kind = v["type"].as_str().unwrap_or(&ev.event);
        let idx = v["output_index"].as_u64().unwrap_or(0);
        let i = idx as usize;
        match kind {
            "response.created" => b.response_id = v["response"]["id"].as_str().map(String::from),
            "response.output_item.added" => {
                let item = &v["item"];
                let k = match item["type"].as_str().unwrap_or_default() {
                    "reasoning" => BlockKind::Thinking,
                    "message" => BlockKind::Text,
                    "function_call" => BlockKind::ToolCall,
                    // Hosted search: shown, never replayed (store is off).
                    "web_search_call" => {
                        self.searches.insert(idx);
                        return Ok(None);
                    }
                    _ => return Ok(None),
                };
                let call_id = item["call_id"].as_str().unwrap_or_default();
                b.start(i, k, call_id, item["name"].as_str().unwrap_or_default()).await;
                self.open.insert(idx, k);
                if k == BlockKind::ToolCall {
                    self.tools += 1;
                }
            }
            "response.reasoning_summary_text.delta"
            | "response.reasoning_text.delta"
            | "response.output_text.delta"
            | "response.refusal.delta"
            | "response.function_call_arguments.delta" => {
                b.delta(i, v["delta"].as_str().unwrap_or_default()).await;
            }
            "response.reasoning_summary_part.done" => b.delta(i, "\n\n").await,
            "response.output_item.done" => {
                let item = &v["item"];
                if self.searches.remove(&idx) {
                    b.start_native(i, item.clone());
                    b.end(i).await;
                    return Ok(None);
                }
                let Some(k) = self.open.remove(&idx) else {
                    return Ok(None);
                };
                match k {
                    BlockKind::Thinking => b.set_replay(i, json!({ "item": item })),
                    BlockKind::Text => {
                        let ann: Vec<Value> = item["content"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .flat_map(|c| c["annotations"].as_array().cloned().unwrap_or_default())
                            .collect();
                        let mut d = json!({ "id": item["id"] });
                        if !ann.is_empty() {
                            d["citations"] = Value::Array(ann);
                        }
                        b.set_replay(i, d);
                    }
                    BlockKind::ToolCall => {
                        // Arguments that only arrive whole (no deltas) are added here.
                        if b.partial_text(i).is_some_and(str::is_empty)
                            && let Some(a) = item["arguments"].as_str()
                        {
                            b.delta(i, a).await;
                        }
                        b.set_replay(i, json!({ "item_id": item["id"] }));
                    }
                    _ => {}
                }
                b.end(i).await;
            }
            "response.completed" | "response.incomplete" => {
                let r = &v["response"];
                b.usage(usage(&r["usage"])).await;
                for k in std::mem::take(&mut self.open).into_keys() {
                    b.end(k as usize).await;
                }
                let stop = match r["incomplete_details"]["reason"].as_str() {
                    Some("max_output_tokens") => StopReason::Length,
                    _ if self.tools > 0 => StopReason::ToolUse,
                    _ => StopReason::Stop,
                };
                return Ok(Some(stop));
            }
            "response.failed" | "error" => {
                let e = if kind == "error" {
                    v.clone()
                } else {
                    v["response"]["error"].clone()
                };
                let msg = e["message"].as_str().unwrap_or("response failed").to_string();
                let code = e["code"].as_str().unwrap_or_default();
                let kind = match code {
                    "context_length_exceeded" => ErrorKind::Overflow,
                    "rate_limit_exceeded" => ErrorKind::RateLimit { retry_after_ms: None },
                    "insufficient_quota" | "usage_limit_reached" => ErrorKind::QuotaExhausted,
                    "invalid_prompt" | "invalid_request_error" => ErrorKind::InvalidRequest,
                    _ => ErrorKind::Transient, // server_error and unknown
                };
                return Err((kind, msg));
            }
            _ => {}
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
            model: "gpt-5.5".into(),
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

    fn reply(provider: &str) -> Message {
        Message::Assistant(AssistantMessage {
            content: vec![
                Block::Thinking {
                    text: "plan".into(),
                    replay: Some(Replay {
                        provider: provider.into(),
                        data: json!({ "item": { "type": "reasoning", "id": "rs_1", "encrypted_content": "ENC" } }),
                    }),
                },
                Block::Text {
                    text: "Reading.".into(),
                    replay: Some(Replay {
                        provider: provider.into(),
                        data: json!({ "id": "msg_1" }),
                    }),
                },
                Block::ToolCall(ToolCall {
                    id: "call_1".into(),
                    name: "read".into(),
                    arguments: json!({"path":"a"}),
                    arguments_error: None,
                    replay: Some(Replay {
                        provider: provider.into(),
                        data: json!({ "item_id": "fc_1" }),
                    }),
                }),
            ],
            stop: StopReason::ToolUse,
            usage: Usage::default(),
            provider: provider.into(),
            model: "gpt-5.5".into(),
            response_id: None,
            error_message: None,
            thinking_level: None,
        })
    }

    fn history(provider: &str) -> Vec<Message> {
        vec![
            Message::System {
                text: "be brief".into(),
            },
            Message::user("hi"),
            reply(provider),
            Message::ToolResult {
                call_id: "call_1".into(),
                tool_name: "read".into(),
                content: vec![Block::text("A")],
                is_error: false,
                details: None,
            },
            Message::System {
                text: "branch changed".into(),
            },
        ]
    }

    #[test]
    fn thinking_off_means_effort_none() {
        let with = |t, c: &Compat| {
            let mut r = req(vec![Message::user("hi")]);
            r.thinking = t;
            build_body(&r, c, "codex")["reasoning"].clone()
        };
        let d = Compat::default();
        assert_eq!(with(Some(ThinkingLevel::Off), &d), json!({ "effort": "none" }));
        assert_eq!(with(None, &d), Value::Null, "non-reasoning model: nothing");
        let copilot = Compat {
            no_effort_none: true,
            ..Default::default()
        };
        assert_eq!(with(Some(ThinkingLevel::Off), &copilot), Value::Null);
    }

    #[test]
    fn body_replays_encrypted_reasoning_to_the_same_model_only() {
        let codex = Compat {
            no_max_tokens: true,
            ..Default::default()
        };
        let b = build_body(&req(history("codex")), &codex, "codex");
        assert_eq!(b["instructions"], "be brief");
        let input = b["input"].as_array().unwrap();
        assert_eq!(
            input[0],
            json!({ "role": "user", "content": [{ "type": "input_text", "text": "hi" }] })
        );
        assert_eq!(input[1]["encrypted_content"], "ENC");
        assert_eq!(input[2]["id"], "msg_1");
        assert_eq!(
            input[3],
            json!({ "type": "function_call", "call_id": "call_1", "name": "read", "arguments": r#"{"path":"a"}"#, "id": "fc_1" })
        );
        assert_eq!(
            input[4],
            json!({ "type": "function_call_output", "call_id": "call_1", "output": "A" })
        );
        assert_eq!(input[5], json!({ "role": "developer", "content": "branch changed" }));
        assert_eq!(b["reasoning"], json!({ "effort": "high", "summary": "auto" }));
        assert!(b.get("max_output_tokens").is_none());
        assert_eq!(
            (b["store"].clone(), b["prompt_cache_key"].clone()),
            (json!(false), json!("sess"))
        );
        // From another namespace: no reasoning item, no ids.
        let b = build_body(&req(history("openai")), &Compat::default(), "codex");
        let input = b["input"].as_array().unwrap();
        assert_eq!(input[1]["type"], "message");
        assert!(input[1].get("id").is_none() && input[2].get("id").is_none());
        assert_eq!(b["max_output_tokens"], 100);
    }

    #[tokio::test]
    async fn stream_reasoning_text_tool_and_usage() {
        let (mut b, rx) = http::channel("codex", "gpt-5.5");
        let mut st = State::default();
        let evs = [
            r#"{"type":"response.created","response":{"id":"resp_1"}}"#,
            r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"reasoning","id":"rs_1"}}"#,
            r#"{"type":"response.reasoning_summary_text.delta","output_index":0,"delta":"Think"}"#,
            r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"reasoning","id":"rs_1","encrypted_content":"ENC","summary":[]}}"#,
            r#"{"type":"response.output_item.added","output_index":1,"item":{"type":"message","id":"msg_1"}}"#,
            r#"{"type":"response.output_text.delta","output_index":1,"delta":"Hi"}"#,
            r#"{"type":"response.output_item.done","output_index":1,"item":{"type":"message","id":"msg_1"}}"#,
            r#"{"type":"response.output_item.added","output_index":2,"item":{"type":"function_call","id":"fc_1","call_id":"call_9","name":"read"}}"#,
            r#"{"type":"response.output_item.done","output_index":2,"item":{"type":"function_call","id":"fc_1","call_id":"call_9","name":"read","arguments":"{\"path\":\"x\"}"}}"#,
            r#"{"type":"response.completed","response":{"id":"resp_1","usage":{"input_tokens":100,"output_tokens":7,"input_tokens_details":{"cached_tokens":64},"output_tokens_details":{"reasoning_tokens":5}}}}"#,
        ];
        let mut stop = None;
        for e in evs {
            let ev = SseEvent {
                event: String::new(),
                data: e.into(),
            };
            if let Some(s) = st.on_event(&ev, &mut b).await.unwrap() {
                stop = Some(s);
            }
        }
        b.finish(stop.unwrap()).await;
        let m = collect(rx).await;
        assert_eq!(m.stop, StopReason::ToolUse);
        assert_eq!((m.usage.input, m.usage.cache_read, m.usage.output), (36, 64, 7));
        assert!(
            matches!(&m.content[0], Block::Thinking { text, replay: Some(r) } if text == "Think" && r.data["item"]["encrypted_content"] == "ENC")
        );
        let call = m.tool_calls().next().unwrap();
        assert_eq!((call.id.as_str(), &call.arguments), ("call_9", &json!({"path":"x"})));
        assert_eq!(call.replay.as_ref().unwrap().data["item_id"], "fc_1");
    }

    #[test]
    fn codex_limit_is_quota() {
        let (k, _) = State::classify(
            429,
            &Default::default(),
            r#"{"error":{"type":"usage_limit_reached","message":"limit"}}"#,
        );
        assert_eq!(k, ErrorKind::QuotaExhausted);
    }
}
