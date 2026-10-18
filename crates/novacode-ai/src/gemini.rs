//! Gemini adapter (L2.8): `generateContent` streaming, with two profiles on
//! one wire: a Gemini API key (`generativelanguage.googleapis.com`) and the
//! Antigravity subscription (Cloud Code `v1internal`, body wrapped in an
//! envelope with the user's project). Thought signatures are replayed to the
//! same model only; foreign calls get Google's documented placeholder signature.

use crate::adapter::{Adapter, Builder, Request};
use crate::http::{self, Failure, Wire};
use crate::sse::SseEvent;
use crate::trace::Trace;
use crate::transform::{self, Target};
use futures_util::future::BoxFuture;
use novacode_types::{Block, BlockKind, ErrorKind, Message, StopReason, StreamEvent, ThinkingLevel, Usage};
use serde_json::{Map, Value, json};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// Google's documented stand-in for history the current model didn't sign.
const SKIP_SIGNATURE: &str = "skip_thought_signature_validator";

pub enum Auth {
    ApiKey(String),
    Antigravity {
        access_token: String,
        project: String,
        user_agent: String,
    },
}

pub struct Gemini {
    http: reqwest::Client,
    base_url: String,
    auth: Auth,
    provider: String,
    trace: Option<Trace>,
}

impl Gemini {
    pub fn new(provider: &str, base_url: &str, auth: Auth, trace: Option<Trace>) -> Self {
        Self {
            http: http::client(),
            base_url: base_url.trim_end_matches('/').into(),
            auth,
            provider: provider.into(),
            trace,
        }
    }

    fn authed(&self, r: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match &self.auth {
            Auth::ApiKey(k) => r.header("x-goog-api-key", k),
            Auth::Antigravity {
                access_token,
                user_agent,
                ..
            } => r.bearer_auth(access_token).header("user-agent", user_agent),
        }
    }

    async fn models(&self) -> Result<Vec<String>, String> {
        let r = match &self.auth {
            Auth::ApiKey(_) => self.http.get(format!("{}/v1beta/models?pageSize=1000", self.base_url)),
            Auth::Antigravity { project, .. } => self
                .http
                .post(format!("{}/v1internal:fetchAvailableModels", self.base_url))
                .header("content-type", "application/json")
                .body(json!({ "project": project }).to_string()),
        };
        let resp = self
            .authed(r)
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
        let mut ids: Vec<String> = match &v["models"] {
            // API key: [{name: "models/gemini-…", supportedGenerationMethods}]
            Value::Array(a) => a
                .iter()
                .filter(|m| {
                    m["supportedGenerationMethods"]
                        .as_array()
                        .is_some_and(|g| g.iter().any(|x| x == "generateContent"))
                })
                .filter_map(|m| m["name"].as_str().map(|n| n.trim_start_matches("models/").to_string()))
                .collect(),
            // Antigravity: {id: {model, displayName, …}}
            Value::Object(o) => o.keys().cloned().collect(),
            _ => Vec::new(),
        };
        ids.sort();
        Ok(ids)
    }
}

impl Adapter for Gemini {
    fn stream(&self, req: Request, cancel: CancellationToken) -> mpsc::Receiver<StreamEvent> {
        let inner = build_request(&req, &self.provider);
        let (url, body) = match &self.auth {
            Auth::ApiKey(_) => (
                format!(
                    "{}/v1beta/models/{}:streamGenerateContent?alt=sse",
                    self.base_url, req.model
                ),
                inner,
            ),
            Auth::Antigravity { project, .. } => (
                format!("{}/v1internal:streamGenerateContent?alt=sse", self.base_url),
                json!({
                    "project": project,
                    "model": req.model,
                    "request": inner,
                    "requestType": "agent",
                    "userAgent": "antigravity",
                    "requestId": format!("agent/{}", novacode_base::new_id()),
                }),
            ),
        };
        let body = serde_json::to_vec(&body).unwrap_or_default();
        let request = self
            .authed(self.http.post(url))
            .header("content-type", "application/json")
            .header("accept", "text/event-stream")
            .body(body.clone());
        let (b, rx) = http::channel(&self.provider, &req.model);
        http::drive(b, rx, request, body, self.trace.clone(), cancel, State::default)
    }

    fn list_models(&self) -> BoxFuture<'_, Result<Vec<String>, String>> {
        Box::pin(self.models())
    }
}

// ---------- request ----------

fn is_claude(model: &str) -> bool {
    model.starts_with("claude-")
}

/// Thinking config by model family (pi-antigravity's table).
fn thinking(model: &str, level: Option<ThinkingLevel>) -> Option<Value> {
    use ThinkingLevel::*;
    let level = level?;
    if level == Off {
        // Pro models can't turn thinking off; leave them at their default.
        return (!model.contains("-pro")).then(|| json!({ "includeThoughts": false, "thinkingBudget": 0 }));
    }
    let budget: i64 = if is_claude(model) {
        match level {
            Minimal | Low => 1024,
            Medium => 4096,
            _ => 16_384,
        }
    } else if model.starts_with("gemini-3") {
        match level {
            Minimal | Low => 1_000,
            Medium => 4_000,
            _ => 10_000,
        }
    } else {
        match level {
            Minimal | Low => 1_024,
            Medium => 4_096,
            _ => -1, // dynamic
        }
    };
    Some(json!({ "includeThoughts": true, "thinkingBudget": budget }))
}

/// The inner `generateContent` body. Pure and deterministic (L1.7 rule 3).
pub fn build_request(req: &Request, ns: &str) -> Value {
    let history = transform::for_target(
        &req.messages_with_tail(),
        &Target {
            provider: ns,
            model: &req.model,
            images: req.images,
            normalize_id: Some(transform::safe_id),
        },
    );
    let skip = history
        .iter()
        .take_while(|m| matches!(m, Message::System { .. }))
        .count();
    let system: Vec<Value> = history[..skip]
        .iter()
        .filter_map(|m| match m {
            Message::System { text } if !text.is_empty() => Some(json!({ "text": text })),
            _ => None,
        })
        .collect();
    let contents = contents(&history[skip..], ns, &req.model);
    let mut body = Map::new();
    body.insert("contents".into(), Value::Array(contents));
    if !system.is_empty() {
        body.insert("systemInstruction".into(), json!({ "role": "user", "parts": system }));
    }
    if !req.tools.is_empty() {
        let decls: Vec<Value> = req
            .tools
            .iter()
            .map(|t| {
                // Claude behind Cloud Code takes the legacy OpenAPI-subset `parameters` (L2.20);
                // Gemini takes JSON Schema.
                if is_claude(&req.model) {
                    json!({ "name": t.name, "description": t.description, "parameters": crate::schema::openapi_subset(&t.schema) })
                } else {
                    let mut schema = t.schema.clone();
                    if let Some(o) = schema.as_object_mut() {
                        o.remove("$schema");
                    }
                    json!({ "name": t.name, "description": t.description, "parametersJsonSchema": schema })
                }
            })
            .collect();
        body.insert("tools".into(), json!([{ "functionDeclarations": decls }]));
    }
    let mut config = Map::new();
    config.insert("maxOutputTokens".into(), json!(req.max_tokens));
    if let Some(t) = thinking(&req.model, req.thinking) {
        config.insert("thinkingConfig".into(), t);
    }
    body.insert("generationConfig".into(), Value::Object(config));
    if let Some(s) = &req.session {
        body.insert("sessionId".into(), json!(s));
    }
    Value::Object(body)
}

fn contents(history: &[Message], ns: &str, model: &str) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::new();
    let mut push = |role: &str, parts: Vec<Value>| {
        if parts.is_empty() {
            return;
        }
        match out.last_mut() {
            Some(last) if last["role"] == role => {
                if let Some(a) = last["parts"].as_array_mut() {
                    a.extend(parts);
                }
            }
            _ => out.push(json!({ "role": role, "parts": parts })),
        }
    };
    for m in history {
        match m {
            Message::System { text } if !text.is_empty() => {
                push(
                    "user",
                    vec![json!({ "text": format!("<system-reminder>\n{text}\n</system-reminder>") })],
                );
            }
            Message::System { .. } | Message::Custom { .. } => {}
            Message::User { content } => push("user", content.iter().filter_map(part).collect()),
            Message::Assistant(a) => {
                let same = a.provider == ns && a.model == model;
                let sig = |r: &Option<novacode_types::Replay>| {
                    r.as_ref()
                        .filter(|r| same && r.provider == ns)
                        .and_then(|r| r.data["sig"].as_str().map(String::from))
                };
                let mut parts = Vec::new();
                let mut first_call = true;
                for b in &a.content {
                    match b {
                        Block::Thinking { text, replay } => {
                            let mut p = json!({ "thought": true, "text": text });
                            if let Some(s) = sig(replay) {
                                p["thoughtSignature"] = json!(s);
                            }
                            parts.push(p);
                        }
                        Block::Text { text, replay } if !text.is_empty() => {
                            let mut p = json!({ "text": text });
                            if let Some(s) = sig(replay) {
                                p["thoughtSignature"] = json!(s);
                            }
                            parts.push(p);
                        }
                        Block::ToolCall(c) => {
                            // The id pairs the call with its result.
                            let fc = json!({ "name": c.name, "args": c.arguments, "id": c.id });
                            let mut p = json!({ "functionCall": fc });
                            // Gemini 3 wants a signature on the first call of a turn.
                            match sig(&c.replay) {
                                Some(s) => p["thoughtSignature"] = json!(s),
                                None if first_call && !is_claude(model) => {
                                    p["thoughtSignature"] = json!(SKIP_SIGNATURE)
                                }
                                None => {}
                            }
                            first_call = false;
                            parts.push(p);
                        }
                        _ => {}
                    }
                }
                push("model", parts);
            }
            Message::ToolResult {
                call_id,
                tool_name,
                content,
                is_error,
                ..
            } => {
                let text: Vec<&str> = content
                    .iter()
                    .filter_map(|b| match b {
                        Block::Text { text, .. } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect();
                let text = text.join("\n");
                let response = if *is_error {
                    json!({ "error": text })
                } else {
                    json!({ "output": text })
                };
                let fr = json!({ "name": tool_name, "response": response, "id": call_id });
                let mut parts = vec![json!({ "functionResponse": fr })];
                parts.extend(
                    content
                        .iter()
                        .filter(|b| matches!(b, Block::Image { .. }))
                        .filter_map(part),
                );
                push("user", parts);
            }
        }
    }
    out
}

fn part(b: &Block) -> Option<Value> {
    match b {
        Block::Text { text, .. } if !text.is_empty() => Some(json!({ "text": text })),
        Block::Image { mime_type, data } => Some(json!({ "inlineData": { "mimeType": mime_type, "data": data } })),
        _ => None,
    }
}

// ---------- stream ----------

#[derive(Default)]
struct State {
    next: usize,
    /// The open text or thinking block: (index, is_thought).
    open: Option<(usize, bool)>,
    tools: usize,
    finish: Option<StopReason>,
}

impl State {
    async fn close(&mut self, b: &mut Builder) {
        if let Some((i, _)) = self.open.take() {
            b.end(i).await;
        }
    }
}

impl Wire for State {
    fn classify(status: u16, headers: &reqwest::header::HeaderMap, body: &str) -> Failure {
        let (kind, msg) = http::classify_openai(status, headers, body);
        let lower = msg.to_ascii_lowercase();
        // Cloud Code: the subscription's model quota is used up.
        let kind = if status == 429 && (lower.contains("quota") || lower.contains("exhausted")) {
            ErrorKind::QuotaExhausted
        } else {
            kind
        };
        (kind, msg)
    }

    async fn at_eof(&mut self, b: &mut Builder) -> Result<StopReason, Failure> {
        self.close(b).await;
        match self.finish.clone() {
            Some(StopReason::Stop) if self.tools > 0 => Ok(StopReason::ToolUse),
            Some(s) => Ok(s),
            None => Err((ErrorKind::Transient, "stream ended without a finish reason".into())),
        }
    }

    async fn on_event(&mut self, ev: &SseEvent, b: &mut Builder) -> Result<Option<StopReason>, Failure> {
        if ev.data.trim().is_empty() {
            return Ok(None);
        }
        let v: Value =
            serde_json::from_str(&ev.data).map_err(|e| (ErrorKind::Transient, format!("bad event json: {e}")))?;
        // Cloud Code wraps the API response.
        let r = if v.get("response").is_some() {
            &v["response"]
        } else {
            &v
        };
        if let Some(e) = r.get("error") {
            let msg = e["message"].as_str().unwrap_or("error").to_string();
            return Err((ErrorKind::Transient, msg));
        }
        if b.response_id.is_none() {
            b.response_id = r["responseId"].as_str().map(String::from);
        }
        if let Some(u) = r.get("usageMetadata") {
            let g = |k: &str| u[k].as_u64().unwrap_or(0);
            let cached = g("cachedContentTokenCount");
            b.usage(Usage {
                input: g("promptTokenCount").saturating_sub(cached),
                output: g("candidatesTokenCount") + g("thoughtsTokenCount"),
                cache_read: cached,
                cache_write: 0,
                reasoning: u["thoughtsTokenCount"].as_u64(),
            })
            .await;
        }
        let Some(cand) = r["candidates"].get(0) else {
            return Ok(None);
        };
        for p in cand["content"]["parts"].as_array().into_iter().flatten() {
            let sig = p["thoughtSignature"].as_str();
            if let Some(fc) = p.get("functionCall") {
                self.close(b).await;
                let i = self.next;
                self.next += 1;
                self.tools += 1;
                let id = fc["id"]
                    .as_str()
                    .map(String::from)
                    .unwrap_or_else(|| format!("call_{}", novacode_base::new_id().simple()));
                b.start(i, BlockKind::ToolCall, &id, fc["name"].as_str().unwrap_or_default())
                    .await;
                b.delta(i, &fc["args"].to_string()).await;
                if let Some(s) = sig {
                    b.set_replay(i, json!({ "sig": s }));
                }
                b.end(i).await;
                continue;
            }
            let Some(text) = p["text"].as_str() else { continue };
            let thought = p["thought"].as_bool().unwrap_or(false);
            if self.open.is_some_and(|(_, t)| t != thought) {
                self.close(b).await;
            }
            if self.open.is_none() {
                let i = self.next;
                self.next += 1;
                let k = if thought { BlockKind::Thinking } else { BlockKind::Text };
                b.start(i, k, "", "").await;
                self.open = Some((i, thought));
            }
            let (i, _) = self.open.unwrap_or_default();
            if !text.is_empty() {
                b.delta(i, text).await;
            }
            if let Some(s) = sig {
                b.set_replay(i, json!({ "sig": s }));
            }
        }
        if let Some(f) = cand["finishReason"].as_str() {
            self.finish = Some(match f {
                "MAX_TOKENS" => StopReason::Length,
                "STOP" | "FINISH_REASON_UNSPECIFIED" => StopReason::Stop,
                // MALFORMED_FUNCTION_CALL, SAFETY, RECITATION…: an error the loop can see.
                other => {
                    self.close(b).await;
                    return Err((ErrorKind::Other, format!("model stopped: {other}")));
                }
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

    fn req(model: &str, messages: Vec<Message>) -> Request {
        Request {
            tail: Vec::new(),
            model: model.into(),
            messages,
            tools: vec![ToolDef {
                name: "read".into(),
                description: "Read".into(),
                schema: json!({"$schema":"x","type":"object"}),
            }],
            max_tokens: 100,
            cache: CacheRetention::Short,
            thinking: Some(ThinkingLevel::Medium),
            images: true,
            session: Some("sess".into()),
        }
    }

    fn history(provider: &str) -> Vec<Message> {
        vec![
            Message::System {
                text: "be brief".into(),
            },
            Message::user("hi"),
            Message::Assistant(AssistantMessage {
                content: vec![Block::ToolCall(ToolCall {
                    id: "c1".into(),
                    name: "read".into(),
                    arguments: json!({"path":"a"}),
                    arguments_error: None,
                    replay: Some(Replay {
                        provider: provider.into(),
                        data: json!({ "sig": "SIG" }),
                    }),
                })],
                stop: StopReason::ToolUse,
                usage: Usage::default(),
                provider: provider.into(),
                model: "gemini-3-flash".into(),
                response_id: None,
                error_message: None,
                thinking_level: None,
            }),
            Message::ToolResult {
                call_id: "c1".into(),
                tool_name: "read".into(),
                content: vec![Block::text("A")],
                is_error: false,
                details: None,
            },
        ]
    }

    #[test]
    fn request_shape_and_signatures() {
        let b = build_request(&req("gemini-3-flash", history("antigravity")), "antigravity");
        assert_eq!(b["systemInstruction"]["parts"][0]["text"], "be brief");
        let c = b["contents"].as_array().unwrap();
        assert_eq!(
            c[1]["parts"][0]["functionCall"],
            json!({ "name": "read", "args": {"path":"a"}, "id": "c1" })
        );
        assert_eq!(c[1]["parts"][0]["thoughtSignature"], "SIG");
        assert_eq!(
            c[2]["parts"][0]["functionResponse"]["response"],
            json!({ "output": "A" })
        );
        let decl = &b["tools"][0]["functionDeclarations"][0];
        assert!(decl["parametersJsonSchema"].get("$schema").is_none());
        assert_eq!(b["generationConfig"]["thinkingConfig"]["thinkingBudget"], 4000);
        // Foreign history gets the placeholder signature; Claude takes `parameters`.
        let b = build_request(&req("gemini-3-flash", history("claude")), "antigravity");
        assert_eq!(b["contents"][1]["parts"][0]["thoughtSignature"], SKIP_SIGNATURE);
        let b = build_request(&req("claude-sonnet-4-6", history("claude")), "antigravity");
        assert!(b["tools"][0]["functionDeclarations"][0].get("parameters").is_some());
    }

    #[tokio::test]
    async fn stream_parts() {
        let (mut b, rx) = http::channel("antigravity", "gemini-3-flash");
        let mut st = State::default();
        for e in [
            r#"{"response":{"responseId":"r1","candidates":[{"content":{"parts":[{"text":"plan","thought":true}]}}]}}"#,
            r#"{"response":{"candidates":[{"content":{"parts":[{"text":"Hi","thoughtSignature":"T"}]}}]}}"#,
            r#"{"response":{"candidates":[{"content":{"parts":[{"functionCall":{"name":"read","args":{"path":"x"}},"thoughtSignature":"S"}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":50,"candidatesTokenCount":5,"thoughtsTokenCount":3,"cachedContentTokenCount":20}}}"#,
        ] {
            st.on_event(
                &SseEvent {
                    event: String::new(),
                    data: e.into(),
                },
                &mut b,
            )
            .await
            .unwrap();
        }
        let stop = st.at_eof(&mut b).await.unwrap();
        b.finish(stop).await;
        let m = collect(rx).await;
        assert_eq!(m.stop, StopReason::ToolUse);
        assert_eq!((m.usage.input, m.usage.cache_read, m.usage.output), (30, 20, 8));
        assert!(matches!(&m.content[0], Block::Thinking { text, .. } if text == "plan"));
        assert!(matches!(&m.content[1], Block::Text { text, replay: Some(r) } if text == "Hi" && r.data["sig"] == "T"));
        let c = m.tool_calls().next().unwrap();
        assert_eq!(
            (&c.arguments, c.replay.as_ref().unwrap().data["sig"].as_str()),
            (&json!({"path":"x"}), Some("S"))
        );
        assert!(c.id.starts_with("call_"));
    }
}
