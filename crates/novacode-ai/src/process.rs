//! Custom providers as programs (L7.6): a profile with `"api": "process"` names a
//! command that speaks line-delimited JSON-RPC 2.0 on stdio. It is started once,
//! on the first call, and serves every call of the session.
//!
//! - `initialize {apiVersion, provider}` → `{models?: [id]}`
//! - `stream {model, messages, tools, maxTokens, thinking, session}` → `{stop: stop|tool_use|length}`,
//!   or an error `{message, data?: {kind}}` (`kind` is an error kind such as `rate_limit`).
//!   While it runs, the program sends `event {id, event}` notifications, where `event` is
//!   `{type: text|thinking, delta}`, `{type: tool_call, id, name, arguments}` or
//!   `{type: usage, input, output, cacheRead?, cacheWrite?}`.
//! - `cancel {id}` (notification): the user stopped the call.
//!
//! Messages and tools are novacode's own JSON (the session format); the shared
//! [`Builder`] turns the events into stream events, as for every built-in adapter.

use crate::adapter::{Adapter, Builder, EVENT_BUFFER, Request};
use futures_util::future::BoxFuture;
use novacode_types::{BlockKind, ErrorKind, StopReason, StreamEvent, Usage};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{OnceCell, mpsc};
use tokio_util::sync::CancellationToken;

pub const API_VERSION: u32 = 1;

/// `command`, `args` and `env` from the provider's profile.
#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

/// Open calls by request id; bounded, so a flooding program is back-pressured (principle 5).
type Calls = Arc<Mutex<HashMap<u64, mpsc::Sender<Msg>>>>;

/// What a running call receives from the reader task.
enum Msg {
    Event(Value),
    Done(Result<Value, Value>),
}

struct Conn {
    tx: mpsc::UnboundedSender<String>,
    calls: Calls,
    models: Vec<String>,
    // shortcut: kill_on_drop stops the program itself, not processes it started; use a group kill if that bites.
    _child: Arc<Mutex<tokio::process::Child>>,
}

pub struct Process(Arc<Inner>);

struct Inner {
    provider: String,
    config: Config,
    conn: OnceCell<Result<Conn, String>>,
    ids: AtomicU64,
}

impl Process {
    pub fn new(provider: &str, config: Config) -> Self {
        Self(Arc::new(Inner {
            provider: provider.into(),
            config,
            conn: OnceCell::new(),
            ids: AtomicU64::new(1),
        }))
    }
}

impl Inner {
    async fn conn(&self) -> Result<&Conn, String> {
        self.conn
            .get_or_init(|| self.start())
            .await
            .as_ref()
            .map_err(Clone::clone)
    }

    async fn start(&self) -> Result<Conn, String> {
        let c = &self.config;
        let mut child = tokio::process::Command::new(&c.command)
            .args(&c.args)
            .envs(&c.env)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| format!("provider `{}`: could not start `{}`: {e}", self.provider, c.command))?;
        let mut stdin = child.stdin.take().ok_or("no stdin")?;
        let stdout = child.stdout.take().ok_or("no stdout")?;
        let (tx, mut rx) = mpsc::unbounded_channel::<String>();
        tokio::spawn(async move {
            while let Some(l) = rx.recv().await {
                if stdin.write_all(format!("{l}\n").as_bytes()).await.is_err() || stdin.flush().await.is_err() {
                    break;
                }
            }
        });
        let calls: Calls = Arc::default();
        let c2 = calls.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(l)) = lines.next_line().await {
                let Ok(v) = serde_json::from_str::<Value>(&l) else {
                    continue;
                };
                if let Some(id) = v["id"].as_u64() {
                    let done = match v.get("error") {
                        Some(e) => Err(e.clone()),
                        None => Ok(v["result"].clone()),
                    };
                    let s = c2.lock().unwrap().remove(&id);
                    if let Some(s) = s {
                        let _ = s.send(Msg::Done(done)).await;
                    }
                } else if v["method"] == "event"
                    && let Some(id) = v["params"]["id"].as_u64()
                {
                    let s = c2.lock().unwrap().get(&id).cloned();
                    if let Some(s) = s {
                        let _ = s.send(Msg::Event(v["params"]["event"].clone())).await;
                    }
                }
            }
            // The program exited: every open call ends with an error.
            let open: Vec<_> = c2.lock().unwrap().drain().map(|(_, s)| s).collect();
            for s in open {
                let _ = s
                    .send(Msg::Done(Err(json!({"message": "the provider program exited"}))))
                    .await;
            }
        });
        let mut conn = Conn {
            tx,
            calls,
            models: Vec::new(),
            _child: Arc::new(Mutex::new(child)),
        };
        let init = json!({"apiVersion": API_VERSION, "provider": self.provider});
        let (_, mut rx) = self.request(&conn, "initialize", init);
        let r = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                match rx.recv().await {
                    Some(Msg::Done(r)) => return r,
                    Some(Msg::Event(_)) => {}
                    None => return Err(json!({"message": "no answer"})),
                }
            }
        })
        .await
        .map_err(|_| format!("provider `{}`: no answer to initialize within 10s", self.provider))?
        .map_err(|e| format!("provider `{}`: initialize failed: {}", self.provider, e["message"]))?;
        conn.models = r["models"]
            .as_array()
            .map(|a| a.iter().filter_map(|m| m.as_str().map(String::from)).collect())
            .unwrap_or_default();
        Ok(conn)
    }

    fn request(&self, conn: &Conn, method: &str, params: Value) -> (u64, mpsc::Receiver<Msg>) {
        let id = self.ids.fetch_add(1, Ordering::Relaxed);
        let (s, r) = mpsc::channel(EVENT_BUFFER);
        conn.calls.lock().unwrap().insert(id, s);
        let _ = conn
            .tx
            .send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}).to_string());
        (id, r)
    }
}

impl Adapter for Process {
    fn stream(&self, req: Request, cancel: CancellationToken) -> mpsc::Receiver<StreamEvent> {
        let (tx, rx) = mpsc::channel(EVENT_BUFFER);
        tokio::spawn(run(self.0.clone(), req, cancel, tx));
        rx
    }

    fn list_models(&self) -> BoxFuture<'_, Result<Vec<String>, String>> {
        Box::pin(async move { Ok(self.0.conn().await?.models.clone()) })
    }
}

async fn run(p: Arc<Inner>, req: Request, cancel: CancellationToken, tx: mpsc::Sender<StreamEvent>) {
    let mut b = Builder::new(tx, &p.provider, &req.model);
    let conn = match p.conn().await {
        Ok(c) => c,
        Err(e) => return b.fail(ErrorKind::Other, e).await,
    };
    let params = json!({
        "model": req.model,
        "messages": req.messages_with_tail(),
        "tools": req.tools,
        "maxTokens": req.max_tokens,
        "thinking": req.thinking,
        "session": req.session,
    });
    let (id, mut rx) = p.request(conn, "stream", params);
    let conn: &Conn = conn;
    // (kind, index) of the open text or thinking block.
    let mut open: Option<(BlockKind, usize)> = None;
    let mut next = 0;
    loop {
        let msg = tokio::select! {
            m = rx.recv() => m,
            _ = cancel.cancelled() => {
                conn.calls.lock().unwrap().remove(&id);
                let _ = conn.tx.send(json!({"jsonrpc": "2.0", "method": "cancel", "params": {"id": id}}).to_string());
                return b.fail(ErrorKind::Aborted, "cancelled").await;
            }
        };
        let ev = match msg {
            Some(Msg::Event(ev)) => ev,
            Some(Msg::Done(Ok(r))) => {
                if let Some((_, i)) = open.take() {
                    b.end(i).await;
                }
                let stop = match r["stop"].as_str() {
                    Some("tool_use") => StopReason::ToolUse,
                    Some("length") => StopReason::Length,
                    _ => StopReason::Stop,
                };
                return b.finish(stop).await;
            }
            Some(Msg::Done(Err(e))) => {
                let kind = serde_json::from_value(json!({"type": e["data"]["kind"]})).unwrap_or(ErrorKind::Other);
                let text = e["message"].as_str().unwrap_or("the provider failed").to_string();
                return b.fail(kind, text).await;
            }
            None => return b.fail(ErrorKind::Other, "the provider program exited").await,
        };
        match ev["type"].as_str() {
            Some(t @ ("text" | "thinking")) => {
                let kind = if t == "text" {
                    BlockKind::Text
                } else {
                    BlockKind::Thinking
                };
                if open.is_some_and(|(k, _)| k != kind)
                    && let Some((_, i)) = open.take()
                {
                    b.end(i).await;
                }
                let i = match open {
                    Some((_, i)) => i,
                    None => {
                        b.start(next, kind, "", "").await;
                        open = Some((kind, next));
                        next += 1;
                        next - 1
                    }
                };
                b.delta(i, ev["delta"].as_str().unwrap_or_default()).await;
            }
            Some("tool_call") => {
                if let Some((_, i)) = open.take() {
                    b.end(i).await;
                }
                let args = match &ev["arguments"] {
                    Value::String(s) => s.clone(),
                    v => v.to_string(),
                };
                b.start(
                    next,
                    BlockKind::ToolCall,
                    ev["id"].as_str().unwrap_or_default(),
                    ev["name"].as_str().unwrap_or_default(),
                )
                .await;
                b.delta(next, &args).await;
                b.end(next).await;
                next += 1;
            }
            Some("usage") => {
                let n = |k: &str| ev[k].as_u64().unwrap_or(0);
                b.usage(Usage {
                    input: n("input"),
                    output: n("output"),
                    cache_read: n("cacheRead"),
                    cache_write: n("cacheWrite"),
                    reasoning: None,
                })
                .await;
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter::{CacheRetention, collect};
    use novacode_types::{Block, Message};

    /// Thinks, says hi, calls a tool; `fail` in the prompt gives a rate-limit error.
    const FAKE: &str = r#"
import json, sys
def send(m): print(json.dumps(m), flush=True)
for line in sys.stdin:
    m = json.loads(line)
    if m["method"] == "initialize":
        send({"jsonrpc": "2.0", "id": m["id"], "result": {"models": ["small", "big"]}}); continue
    if m["method"] != "stream": continue
    p, i = m["params"], m["id"]
    ev = lambda e: send({"jsonrpc": "2.0", "method": "event", "params": {"id": i, "event": e}})
    if "fail" in json.dumps(p["messages"]):
        send({"jsonrpc": "2.0", "id": i, "error": {"code": 1, "message": "slow down", "data": {"kind": "rate_limit"}}}); continue
    ev({"type": "thinking", "delta": "hmm"})
    ev({"type": "text", "delta": "hi "}); ev({"type": "text", "delta": p["model"]})
    ev({"type": "tool_call", "id": "c1", "name": "read", "arguments": {"path": "a"}})
    ev({"type": "usage", "input": 7, "output": 3})
    send({"jsonrpc": "2.0", "id": i, "result": {"stop": "tool_use"}})
"#;

    fn req(text: &str) -> Request {
        Request {
            model: "small".into(),
            messages: vec![Message::user(text)],
            tools: vec![],
            max_tokens: 100,
            cache: CacheRetention::Short,
            thinking: None,
            images: false,
            session: None,
            tail: vec![],
        }
    }

    #[tokio::test]
    async fn streams_through_the_builder() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("p.py");
        std::fs::write(&f, FAKE).unwrap();
        let p = Process::new(
            "mine",
            Config {
                command: "python3".into(),
                args: vec![f.display().to_string()],
                env: Default::default(),
            },
        );
        assert_eq!(p.list_models().await.unwrap(), ["small", "big"]);
        let m = collect(p.stream(req("hello"), CancellationToken::new())).await;
        assert_eq!(m.stop, StopReason::ToolUse);
        assert_eq!(m.provider, "mine");
        assert_eq!(m.usage.input, 7);
        assert!(matches!(&m.content[0], Block::Thinking { text, .. } if text == "hmm"));
        assert!(matches!(&m.content[1], Block::Text { text, .. } if text == "hi small"));
        assert!(
            matches!(&m.content[2], Block::ToolCall(c) if c.name == "read" && c.arguments["path"] == "a"),
            "{:?}",
            m.content
        );
        let e = collect(p.stream(req("fail please"), CancellationToken::new())).await;
        assert!(
            matches!(
                e.stop,
                StopReason::Error {
                    kind: ErrorKind::RateLimit { .. }
                }
            ),
            "{:?}",
            e.stop
        );
        assert_eq!(e.error_message.as_deref(), Some("slow down"));
    }

    #[tokio::test]
    async fn a_missing_program_is_an_error() {
        let p = Process::new(
            "mine",
            Config {
                command: "/nonexistent/provider".into(),
                args: vec![],
                env: Default::default(),
            },
        );
        let m = collect(p.stream(req("hi"), CancellationToken::new())).await;
        assert!(m.error_message.unwrap().contains("could not start"));
    }
}
