//! ACP adapter (L8.12): `novacode --acp` speaks the Agent Client Protocol v1
//! (JSON-RPC 2.0, one message per line on stdin/stdout) to an editor such as Zed.
//! Each ACP session is a `novacode --rpc` child in the session's folder, driven over
//! the L8.7 wire protocol, so `novacode attach <id>` can join it from a terminal.

use crate::wire::{ClientCmd, Request, ServerMsg, State};
use agent_client_protocol_schema::ProtocolVersion;
use agent_client_protocol_schema::v1 as acp;
use base64::Engine;
use novacode_base::paths::Paths;
use novacode_tui::dialog::Answer;
use novacode_types::{AgentEvent, Block, BlockKind, Message, Question, Reply, StopReason, StreamEvent, ThinkingLevel};
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot};

type Lines = tokio::io::Lines<BufReader<tokio::process::ChildStdout>>;

/// What a JSON-RPC request we sent came back with: `result`, or `error`.
type ClientReply = Result<Value, Value>;

/// `model`: `-m` for every session (else each folder's settings choose).
pub async fn run(paths: Paths, model: Option<String>) -> anyhow::Result<ExitCode> {
    let (out, mut out_rx) = mpsc::unbounded_channel::<String>();
    let writer = tokio::spawn(async move {
        let mut stdout = tokio::io::stdout();
        while let Some(mut l) = out_rx.recv().await {
            l.push('\n');
            if stdout.write_all(l.as_bytes()).await.is_err() || stdout.flush().await.is_err() {
                break;
            }
        }
    });
    let acp = Arc::new(Acp {
        out,
        next: AtomicU64::new(1),
        waiting: Mutex::default(),
        closed: AtomicBool::new(false),
        sessions: Mutex::default(),
        caps: Mutex::default(),
        paths,
        model,
    });
    let mut requests = tokio::task::JoinSet::new();
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    while let Ok(Some(l)) = lines.next_line().await {
        let Ok(msg) = serde_json::from_str::<Value>(&l) else {
            acp.send(json!({"jsonrpc": "2.0", "id": null, "error": {"code": -32700, "message": "parse error"}}));
            continue;
        };
        let id = msg.get("id").cloned();
        match (msg.get("method").and_then(Value::as_str), id) {
            (Some(method), Some(id)) => {
                let (acp, method, params) = (acp.clone(), method.to_string(), msg["params"].clone());
                requests.spawn(async move {
                    let r = acp.request_in(&method, params).await;
                    acp.send(match r {
                        Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
                        Err((code, message)) => {
                            json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
                        }
                    });
                });
            }
            (Some(method), None) => acp.notification_in(method, &msg["params"]),
            (None, Some(id)) => {
                let r = match msg.get("error") {
                    Some(e) => Err(e.clone()),
                    None => Ok(msg.get("result").cloned().unwrap_or(Value::Null)),
                };
                if let Some((_, tx)) = id.as_u64().and_then(|n| acp.waiting.lock().unwrap().remove(&n)) {
                    let _ = tx.send(r);
                }
            }
            (None, None) => {}
        }
        while requests.try_join_next().is_some() {}
    }
    acp.closed.store(true, Ordering::SeqCst);
    acp.waiting.lock().unwrap().clear();
    requests.shutdown().await;
    // The editor left: end every session cleanly (its processes, its session file).
    let all: Vec<Arc<Sess>> = acp.sessions.lock().unwrap().drain().map(|(_, s)| s).collect();
    for s in all {
        s.close().await;
    }
    drop(acp);
    let mut writer = writer;
    if tokio::time::timeout(std::time::Duration::from_secs(1), &mut writer)
        .await
        .is_err()
    {
        writer.abort();
    }
    Ok(ExitCode::SUCCESS)
}

#[derive(Default)]
struct Caps {
    /// The client renders `elicitation/create` forms (`ask_user`).
    form: bool,
    /// The client shows `notice` updates.
    notices: bool,
}

struct Acp {
    out: mpsc::UnboundedSender<String>,
    next: AtomicU64,
    waiting: Mutex<HashMap<u64, (String, oneshot::Sender<ClientReply>)>>,
    closed: AtomicBool,
    sessions: Mutex<HashMap<String, Arc<Sess>>>,
    caps: Mutex<Caps>,
    paths: Paths,
    model: Option<String>,
}

type RpcError = (i64, String);

fn bad(e: impl std::fmt::Display) -> RpcError {
    (-32602, format!("invalid params: {e}"))
}

fn internal(e: impl std::fmt::Display) -> RpcError {
    (-32603, e.to_string())
}

fn to_value<T: Serialize>(v: T) -> Value {
    serde_json::to_value(v).unwrap_or_default()
}

impl Acp {
    fn send(&self, v: Value) {
        let _ = self.out.send(v.to_string());
    }

    fn update(&self, session: &str, u: acp::SessionUpdate) {
        let n = acp::SessionNotification::new(session.to_string(), u);
        self.send(json!({"jsonrpc": "2.0", "method": "session/update", "params": n}));
    }

    fn notice(&self, session: &str, text: &str, error: bool) {
        if !self.caps.lock().unwrap().notices {
            return;
        }
        let sev = if error {
            acp::NoticeSeverity::Error
        } else {
            acp::NoticeSeverity::Info
        };
        self.update(session, acp::SessionUpdate::Notice(acp::Notice::new(sev, text)));
    }

    /// A request to the client (permission, form); waits for its answer.
    async fn ask_client(&self, session: &Sess, method: &str, params: impl Serialize) -> ClientReply {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        {
            let mut waiting = self.waiting.lock().unwrap();
            if self.closed.load(Ordering::SeqCst) || session.closed.load(Ordering::SeqCst) {
                return Err(Value::Null);
            }
            waiting.insert(id, (session.id.clone(), tx));
        }
        self.send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        rx.await.unwrap_or(Err(Value::Null))
    }

    fn session(&self, id: &str) -> Result<Arc<Sess>, RpcError> {
        self.sessions
            .lock()
            .unwrap()
            .get(id)
            .cloned()
            .ok_or_else(|| bad(format!("no session {id}")))
    }

    fn notification_in(&self, method: &str, params: &Value) {
        if method == "session/cancel"
            && let Ok(n) = serde_json::from_value::<acp::CancelNotification>(params.clone())
            && let Ok(s) = self.session(&n.session_id.0)
        {
            s.cmd(ClientCmd::Abort { drop_queued: false });
        }
    }

    async fn request_in(self: &Arc<Self>, method: &str, params: Value) -> Result<Value, RpcError> {
        match method {
            "initialize" => {
                let r: acp::InitializeRequest = serde_json::from_value(params).map_err(bad)?;
                let c = &r.client_capabilities;
                *self.caps.lock().unwrap() = Caps {
                    form: c.elicitation.as_ref().is_some_and(|e| e.supports_form()),
                    notices: c.session.as_ref().is_some_and(|s| s.notices.is_some()),
                };
                let caps = acp::AgentCapabilities::new()
                    .load_session(true)
                    .prompt_capabilities(acp::PromptCapabilities::new().image(true))
                    .session_capabilities(
                        acp::SessionCapabilities::new()
                            .list(acp::SessionListCapabilities::new())
                            .close(acp::SessionCloseCapabilities::new()),
                    );
                Ok(to_value(
                    acp::InitializeResponse::new(ProtocolVersion::V1)
                        .agent_capabilities(caps)
                        .agent_info(acp::Implementation::new("novacode", env!("CARGO_PKG_VERSION"))),
                ))
            }
            "session/new" => {
                let r: acp::NewSessionRequest = serde_json::from_value(params).map_err(bad)?;
                let s = self.open(&r.cwd, None).await?;
                Ok(to_value(
                    acp::NewSessionResponse::new(s.id.clone()).config_options(s.options()),
                ))
            }
            "session/load" => {
                let r: acp::LoadSessionRequest = serde_json::from_value(params).map_err(bad)?;
                let old = self.sessions.lock().unwrap().get(&*r.session_id.0).cloned();
                if let Some(old) = old {
                    old.close().await;
                }
                let s = self.open(&r.cwd, Some(&r.session_id.0)).await?;
                Ok(to_value(acp::LoadSessionResponse::new().config_options(s.options())))
            }
            "session/list" => {
                let r: acp::ListSessionsRequest = serde_json::from_value(params).map_err(bad)?;
                let cwd = r.cwd.unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
                Ok(to_value(acp::ListSessionsResponse::new(list(&self.paths, &cwd))))
            }
            "session/prompt" => {
                let r: acp::PromptRequest = serde_json::from_value(params).map_err(bad)?;
                let s = self.session(&r.session_id.0)?;
                let text = prompt_text(&r.prompt).map_err(internal)?;
                let stop = s.prompt(text).await.map_err(internal)?;
                Ok(to_value(acp::PromptResponse::new(stop)))
            }
            "session/set_config_option" => {
                let r: acp::SetSessionConfigOptionRequest = serde_json::from_value(params).map_err(bad)?;
                let s = self.session(&r.session_id.0)?;
                let acp::SessionConfigOptionValue::ValueId { value } = r.value else {
                    return Err(bad("expected a value id"));
                };
                s.set_option(&r.config_id.0, &value.0).await.map_err(internal)?;
                Ok(to_value(acp::SetSessionConfigOptionResponse::new(s.options())))
            }
            "session/close" => {
                let r: acp::CloseSessionRequest = serde_json::from_value(params).map_err(bad)?;
                let s = self.sessions.lock().unwrap().get(&*r.session_id.0).cloned();
                if let Some(s) = s {
                    s.close().await;
                }
                Ok(json!({}))
            }
            _ => Err((-32601, format!("method not found: {method}"))),
        }
    }

    /// Start a `--rpc` child in `cwd` (resuming `resume`), replay its history, and
    /// keep translating its messages until it ends.
    async fn open(self: &Arc<Self>, cwd: &Path, resume: Option<&str>) -> Result<Arc<Sess>, RpcError> {
        let mut cmd = tokio::process::Command::new(crate::agents::own_exe());
        cmd.kill_on_drop(true)
            .arg("--rpc")
            .current_dir(cwd)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit());
        if let Some(id) = resume {
            cmd.args(["--resume", id]);
        }
        if let Some(m) = &self.model {
            cmd.args(["-m", m]);
        }
        let mut child = cmd
            .spawn()
            .map_err(|e| internal(format!("starting novacode --rpc: {e}")))?;
        let (stdin, stdout) = (child.stdin.take(), child.stdout.take());
        let (Some(mut stdin), Some(stdout)) = (stdin, stdout) else {
            return Err(internal("no pipes to the session process"));
        };
        let mut lines = BufReader::new(stdout).lines();
        let state = match next_msg(&mut lines).await {
            Some(ServerMsg::Hello { state, .. }) => state,
            _ => {
                let st = child.wait().await.map(|s| s.to_string()).unwrap_or_default();
                return Err(internal(format!(
                    "the session process ended before it was ready ({st}); see the agent log"
                )));
            }
        };
        let (tx, mut rx) = mpsc::unbounded_channel::<String>();
        tokio::spawn(async move {
            while let Some(l) = rx.recv().await {
                if stdin.write_all(l.as_bytes()).await.is_err() || stdin.flush().await.is_err() {
                    break;
                }
            }
        });
        let s = Arc::new(Sess {
            id: state.session.clone(),
            cwd: cwd.to_path_buf(),
            acp: Arc::downgrade(self),
            tx,
            next: AtomicU64::new(1),
            prompts: Mutex::default(),
            replies: Mutex::default(),
            child: tokio::sync::Mutex::new(Some(child)),
            closed: AtomicBool::new(false),
            view: Mutex::new(View {
                state,
                ..Default::default()
            }),
        });
        // History comes right after hello: replay it before answering `session/load`.
        if let Some(ServerMsg::History { messages }) = next_msg(&mut lines).await
            && resume.is_some()
        {
            s.replay(&messages);
        }
        self.sessions.lock().unwrap().insert(s.id.clone(), s.clone());
        let reader = s.clone();
        tokio::spawn(async move {
            while let Some(m) = next_msg(&mut lines).await {
                reader.on_msg(m);
            }
            reader.ended();
        });
        let bg = s.clone();
        tokio::spawn(async move {
            bg.cmd(ClientCmd::ListModels);
            if let Ok(data) = bg.call(ClientCmd::GetCommands).await {
                bg.commands(&data);
            }
        });
        Ok(s)
    }
}

async fn next_msg(lines: &mut Lines) -> Option<ServerMsg> {
    loop {
        let l = lines.next_line().await.ok()??;
        match serde_json::from_str(&l) {
            Ok(m) => return Some(m),
            Err(e) => tracing::warn!(%e, "acp: unreadable line from the session process"),
        }
    }
}

/// What the translator remembers about one session.
#[derive(Default)]
struct View {
    state: State,
    /// `(provider/model, label)` from `list_models`.
    models: Vec<(String, String)>,
    /// Kind of each streamed block of the current reply, by index.
    blocks: HashMap<usize, BlockKind>,
    /// Tool calls shown but not finished.
    calls: Vec<String>,
    /// Tools whose diff stays as their content when they finish.
    diffs: Vec<String>,
    last_error: Option<String>,
    compactions: u64,
    /// What the config options and usage last said, to send only changes.
    sent_options: Value,
    sent_usage: (u64, u64),
}

struct Sess {
    id: String,
    cwd: PathBuf,
    acp: std::sync::Weak<Acp>,
    tx: mpsc::UnboundedSender<String>,
    next: AtomicU64,
    /// Wire prompt id → the `session/prompt` waiting for its `agent_end`.
    prompts: Mutex<HashMap<String, oneshot::Sender<Result<acp::StopReason, String>>>>,
    /// Wire request id → the command waiting for its `response`.
    replies: Mutex<HashMap<String, oneshot::Sender<Result<Value, String>>>>,
    child: tokio::sync::Mutex<Option<tokio::process::Child>>,
    closed: AtomicBool,
    view: Mutex<View>,
}

impl Sess {
    fn acp(&self) -> Option<Arc<Acp>> {
        self.acp.upgrade()
    }

    fn update(&self, u: acp::SessionUpdate) {
        if let Some(a) = self.acp() {
            a.update(&self.id, u);
        }
    }

    fn cmd(&self, cmd: ClientCmd) -> String {
        let id = format!("a{}", self.next.fetch_add(1, Ordering::Relaxed));
        let req = Request {
            id: Some(id.clone()),
            cmd,
        };
        let _ = self.tx.send(crate::wire::line(&req));
        id
    }

    async fn call(&self, cmd: ClientCmd) -> Result<Value, String> {
        let (tx, rx) = oneshot::channel();
        let id = format!("a{}", self.next.fetch_add(1, Ordering::Relaxed));
        self.replies.lock().unwrap().insert(id.clone(), tx);
        let _ = self.tx.send(crate::wire::line(&Request { id: Some(id), cmd }));
        rx.await.unwrap_or_else(|_| Err("the session ended".into()))
    }

    /// Send a prompt; resolves when the run that takes it ends.
    async fn prompt(&self, text: String) -> Result<acp::StopReason, String> {
        let (tx, rx) = oneshot::channel();
        let id = format!("a{}", self.next.fetch_add(1, Ordering::Relaxed));
        self.prompts.lock().unwrap().insert(id.clone(), tx);
        let (rtx, rrx) = oneshot::channel();
        self.replies.lock().unwrap().insert(id.clone(), rtx);
        let req = Request {
            id: Some(id.clone()),
            cmd: ClientCmd::Prompt { text, raw: false },
        };
        let _ = self.tx.send(crate::wire::line(&req));
        let data = rrx.await.unwrap_or_else(|_| Err("the session ended".into()));
        match data {
            // A slash command or `!cmd` ran here: no run, so no `agent_end`.
            Ok(d) if d["disposition"] == "handled" => {
                self.prompts.lock().unwrap().remove(&id);
                Ok(acp::StopReason::EndTurn)
            }
            Ok(_) => rx.await.unwrap_or_else(|_| Err("the session ended".into())),
            Err(e) => {
                self.prompts.lock().unwrap().remove(&id);
                Err(e)
            }
        }
    }

    async fn set_option(&self, id: &str, value: &str) -> Result<(), String> {
        match id {
            "model" => {
                self.call(ClientCmd::SetModel { model: value.into() }).await?;
            }
            "thought_level" => {
                let level = ThinkingLevel::parse(value).ok_or_else(|| format!("unknown thinking level `{value}`"))?;
                self.call(ClientCmd::SetThinking { level }).await?;
            }
            "mode" => {
                if value != "off" && !self.view.lock().unwrap().state.modes.iter().any(|mode| mode == value) {
                    return Err(format!("unknown mode `{value}`"));
                }
                let text = format!("/mode {value}");
                self.call(ClientCmd::Prompt { text, raw: false }).await?;
            }
            _ => return Err(format!("unknown option `{id}`")),
        }
        let state = self.call(ClientCmd::GetState).await?;
        self.on_state(serde_json::from_value(state).map_err(|e| format!("invalid session state: {e}"))?);
        Ok(())
    }

    fn cancel_client_requests(&self) {
        self.closed.store(true, Ordering::SeqCst);
        if let Some(a) = self.acp() {
            a.waiting.lock().unwrap().retain(|_, (session, _)| session != &self.id);
        }
    }

    async fn close(&self) {
        self.cancel_client_requests();
        if let Some(mut c) = self.child.lock().await.take() {
            // Bound the acknowledgment as well as process exit: either can stall.
            let shutdown = async {
                let _ = self.call(ClientCmd::Shutdown).await;
                let _ = c.wait().await;
            };
            if tokio::time::timeout(std::time::Duration::from_secs(10), shutdown)
                .await
                .is_err()
            {
                let _ = c.kill().await;
            }
        }
        self.ended();
    }

    /// The child is gone: everything still waiting fails.
    fn ended(&self) {
        self.cancel_client_requests();
        for (_, tx) in self.prompts.lock().unwrap().drain() {
            let _ = tx.send(Err("the session process ended".into()));
        }
        for (_, tx) in self.replies.lock().unwrap().drain() {
            let _ = tx.send(Err("the session process ended".into()));
        }
        if let Some(a) = self.acp() {
            let mut sessions = a.sessions.lock().unwrap();
            if sessions.get(&self.id).is_some_and(|s| std::ptr::eq(s.as_ref(), self)) {
                sessions.remove(&self.id);
            }
        }
    }

    fn options(&self) -> Vec<acp::SessionConfigOption> {
        options(&self.view.lock().unwrap())
    }

    fn commands(&self, data: &Value) {
        let cmds = data["commands"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|c| {
                let name = c["name"].as_str()?.trim_start_matches('/');
                Some(acp::AvailableCommand::new(
                    name,
                    c["description"].as_str().unwrap_or_default(),
                ))
            })
            .collect();
        self.update(acp::SessionUpdate::AvailableCommandsUpdate(
            acp::AvailableCommandsUpdate::new(cmds),
        ));
    }

    fn on_msg(self: &Arc<Self>, m: ServerMsg) {
        match m {
            ServerMsg::Event { event } => self.on_event(event),
            ServerMsg::Response {
                id, ok, data, error, ..
            } => {
                if let Some(tx) = id.and_then(|id| self.replies.lock().unwrap().remove(&id)) {
                    let _ = tx.send(if ok { Ok(data) } else { Err(error.unwrap_or_default()) });
                }
            }
            ServerMsg::State { state } => self.on_state(state),
            ServerMsg::Ui { ui } => self.on_ui(&ui),
            ServerMsg::Approve {
                id,
                tool,
                detail,
                reason,
                ..
            } => {
                let me = self.clone();
                tokio::spawn(async move { me.approve(id, tool, detail, reason).await });
            }
            ServerMsg::Ask { id, questions } => {
                let me = self.clone();
                tokio::spawn(async move { me.ask(id, questions).await });
            }
            _ => {}
        }
    }

    fn on_state(&self, state: State) {
        let mut v = self.view.lock().unwrap();
        v.state = state;
        let opts = options(&v);
        let opts_v = to_value(&opts);
        let usage = (v.state.context_tokens, v.state.window_tokens);
        let opts_changed = opts_v != v.sent_options;
        let usage_changed = usage != v.sent_usage && usage.1 > 0;
        v.sent_options = opts_v;
        v.sent_usage = usage;
        drop(v);
        if opts_changed {
            self.update(acp::SessionUpdate::ConfigOptionUpdate(acp::ConfigOptionUpdate::new(
                opts,
            )));
        }
        if usage_changed {
            self.update(acp::SessionUpdate::UsageUpdate(acp::UsageUpdate::new(usage.0, usage.1)));
        }
    }

    fn on_ui(&self, ui: &Value) {
        match ui["kind"].as_str() {
            Some("models") => {
                let models = ui["result"]["Ok"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|m| Some((m["value"].as_str()?.to_string(), m["label"].as_str()?.to_string())))
                    .collect();
                let state = {
                    let mut v = self.view.lock().unwrap();
                    v.models = models;
                    v.state.clone()
                };
                self.on_state(state);
            }
            Some("notice") => {
                if let Some(a) = self.acp() {
                    a.notice(&self.id, ui["text"].as_str().unwrap_or_default(), ui["error"] == true);
                }
            }
            // A command's output (`/context`, `/mcp`): shown as the reply.
            Some("page") => {
                let text = format!(
                    "**{}**\n\n{}\n",
                    ui["title"].as_str().unwrap_or_default(),
                    ui["text"].as_str().unwrap_or_default()
                );
                self.update(acp::SessionUpdate::AgentMessageChunk(acp::ContentChunk::new(
                    text.into(),
                )));
            }
            _ => {}
        }
    }

    fn on_event(&self, e: AgentEvent) {
        match e {
            AgentEvent::Stream(s) => match s {
                StreamEvent::BlockStart { index, kind, .. } => {
                    self.view.lock().unwrap().blocks.insert(index, kind);
                }
                StreamEvent::Delta { index, text } => {
                    let kind = self.view.lock().unwrap().blocks.get(&index).copied();
                    let chunk = acp::ContentChunk::new(text.into());
                    match kind {
                        Some(BlockKind::Text) => self.update(acp::SessionUpdate::AgentMessageChunk(chunk)),
                        Some(BlockKind::Thinking) => self.update(acp::SessionUpdate::AgentThoughtChunk(chunk)),
                        _ => {}
                    }
                }
                StreamEvent::BlockEnd {
                    block: Block::ToolCall(call),
                    ..
                } => self.tool_call(&call.id, &call.name, &call.arguments, acp::ToolCallStatus::Pending),
                StreamEvent::Error(m) => self.view.lock().unwrap().last_error = m.error_message,
                StreamEvent::Done(_) => self.view.lock().unwrap().blocks.clear(),
                StreamEvent::Notice { text } => {
                    if let Some(a) = self.acp() {
                        a.notice(&self.id, &text, false);
                    }
                }
                _ => {}
            },
            AgentEvent::ToolStart { call } => {
                let known = self.view.lock().unwrap().calls.contains(&call.id);
                if !known {
                    self.tool_call(&call.id, &call.name, &call.arguments, acp::ToolCallStatus::InProgress);
                }
                let f = acp::ToolCallUpdateFields::new().status(acp::ToolCallStatus::InProgress);
                self.update(acp::SessionUpdate::ToolCallUpdate(acp::ToolCallUpdate::new(call.id, f)));
            }
            AgentEvent::Message(Message::ToolResult {
                call_id,
                content,
                is_error,
                ..
            }) => self.tool_result(&call_id, &content, is_error),
            AgentEvent::AgentEnd { stop, prompts } => {
                let last_error = self.view.lock().unwrap().last_error.take();
                let r = match stop {
                    StopReason::Stop | StopReason::ToolUse => Ok(acp::StopReason::EndTurn),
                    StopReason::Length => Ok(acp::StopReason::MaxTokens),
                    StopReason::Aborted => Ok(acp::StopReason::Cancelled),
                    StopReason::Error { .. } => Err(last_error.unwrap_or_else(|| "the model call failed".into())),
                };
                for id in prompts.into_iter().flatten() {
                    if let Some(tx) = self.prompts.lock().unwrap().remove(&id) {
                        let _ = tx.send(r.clone());
                    }
                }
            }
            AgentEvent::SessionNamed { name } => {
                self.update(acp::SessionUpdate::SessionInfoUpdate(
                    acp::SessionInfoUpdate::new().title(name),
                ));
            }
            AgentEvent::Compacting { .. } => {
                let n = {
                    let mut v = self.view.lock().unwrap();
                    v.compactions += 1;
                    v.compactions
                };
                let u = acp::CompactionUpdate::new(format!("c{n}"), acp::CompactionStatus::InProgress);
                self.update(acp::SessionUpdate::CompactionUpdate(u));
            }
            AgentEvent::Compacted { .. } => {
                let n = self.view.lock().unwrap().compactions;
                let u = acp::CompactionUpdate::new(format!("c{n}"), acp::CompactionStatus::Completed);
                self.update(acp::SessionUpdate::CompactionUpdate(u));
            }
            AgentEvent::Retry { delay_ms, message, .. } => {
                if let Some(a) = self.acp() {
                    a.notice(
                        &self.id,
                        &format!("retrying in {} s: {message}", delay_ms.div_ceil(1000)),
                        true,
                    );
                }
            }
            _ => {}
        }
    }

    /// Show a tool call: title, kind, diffs for edits, and where it works (follow-along).
    fn tool_call(&self, id: &str, tool: &str, args: &Value, status: acp::ToolCallStatus) {
        let diffs = diffs(tool, args, &self.cwd);
        let mut v = self.view.lock().unwrap();
        v.calls.push(id.to_string());
        if !diffs.is_empty() {
            v.diffs.push(id.to_string());
        }
        drop(v);
        let call = acp::ToolCall::new(id.to_string(), title(tool, args))
            .kind(kind(tool))
            .status(status)
            .content(diffs)
            .locations(locations(tool, args, &self.cwd))
            .raw_input(args.clone());
        self.update(acp::SessionUpdate::ToolCall(call));
    }

    fn tool_result(&self, id: &str, content: &[Block], is_error: bool) {
        let mut v = self.view.lock().unwrap();
        v.calls.retain(|c| c != id);
        let keep_diff = !is_error && v.diffs.contains(&id.to_string());
        v.diffs.retain(|d| d != id);
        drop(v);
        let status = if is_error {
            acp::ToolCallStatus::Failed
        } else {
            acp::ToolCallStatus::Completed
        };
        let mut f = acp::ToolCallUpdateFields::new().status(status);
        if !keep_diff {
            f = f.content(output(content));
        }
        self.update(acp::SessionUpdate::ToolCallUpdate(acp::ToolCallUpdate::new(
            id.to_string(),
            f,
        )));
    }

    /// A permission card (L6.1), independent of streamed calls: the wire has no call id.
    async fn approve(&self, id: u64, tool: String, detail: String, reason: String) {
        let Some(a) = self.acp() else { return };
        // Never borrow another call's diff: all same-tool approvals precede ToolStart.
        let call = format!("approval-{id}");
        let text = format!("{tool}: {detail}\n\n{reason}");
        let fields = acp::ToolCallUpdateFields::new()
            .title(title_text(&tool, &detail))
            .kind(kind(&tool))
            .status(acp::ToolCallStatus::Pending)
            .content(vec![text.into()]);
        let options = vec![
            acp::PermissionOption::new("once", "Allow once", acp::PermissionOptionKind::AllowOnce),
            acp::PermissionOption::new(
                "session",
                "Allow for this session",
                acp::PermissionOptionKind::AllowAlways,
            ),
            acp::PermissionOption::new(
                "always",
                "Always allow (saves a rule)",
                acp::PermissionOptionKind::AllowAlways,
            ),
            acp::PermissionOption::new("deny", "Deny", acp::PermissionOptionKind::RejectOnce),
        ];
        let req = acp::RequestPermissionRequest::new(
            self.id.clone(),
            acp::ToolCallUpdate::new(call.clone(), fields),
            options,
        );
        let answer = match a.ask_client(self, "session/request_permission", req).await {
            Ok(v) => match serde_json::from_value::<acp::RequestPermissionResponse>(v).map(|r| r.outcome) {
                Ok(acp::RequestPermissionOutcome::Selected(s)) => match &*s.option_id.0 {
                    "once" => Answer::Once,
                    "session" => Answer::Session,
                    "always" => Answer::Always,
                    _ => Answer::Deny(None),
                },
                _ => Answer::Deny(None),
            },
            Err(_) => Answer::Deny(None),
        };
        let status = if matches!(answer, Answer::Deny(_)) {
            acp::ToolCallStatus::Failed
        } else {
            acp::ToolCallStatus::Completed
        };
        self.update(acp::SessionUpdate::ToolCallUpdate(acp::ToolCallUpdate::new(
            call,
            acp::ToolCallUpdateFields::new().status(status),
        )));
        self.cmd(ClientCmd::Approval { approval: id, answer });
    }

    /// `ask_user` (L5.21) as an `elicitation/create` form.
    async fn ask(&self, id: u64, questions: Vec<Question>) {
        let Some(a) = self.acp() else { return };
        if !a.caps.lock().unwrap().form {
            // Left open: `novacode attach <id>` in a terminal can answer it.
            a.notice(
                &self.id,
                &format!(
                    "The agent asks a question this editor can't show; answer it with `novacode attach {}`.",
                    self.id
                ),
                true,
            );
            return;
        }
        let message = match questions.as_slice() {
            [q] => q.question.clone(),
            qs => format!("The agent has {} questions.", qs.len()),
        };
        let mode = acp::ElicitationFormMode::new(acp::ElicitationSessionScope::new(self.id.clone()), form(&questions));
        let req = acp::CreateElicitationRequest::new(mode, message);
        let replies = match a.ask_client(self, "elicitation/create", req).await {
            Ok(v) => match serde_json::from_value::<acp::CreateElicitationResponse>(v).map(|r| r.action) {
                Ok(acp::ElicitationAction::Accept(acc)) => Some(replies(&questions, &acc.content.unwrap_or_default())),
                _ => None,
            },
            Err(_) => None,
        };
        self.cmd(ClientCmd::Answer { ask: id, replies });
    }

    /// A resumed session's messages, as the editor would have seen them live.
    fn replay(&self, messages: &[Message]) {
        let mut results = HashMap::new();
        for m in messages {
            if let Message::ToolResult {
                call_id,
                content,
                is_error,
                ..
            } = m
            {
                results.insert(call_id.clone(), (content, *is_error));
            }
        }
        for m in messages {
            match m {
                Message::User { content } => {
                    let text: Vec<&str> = content
                        .iter()
                        .filter_map(|b| match b {
                            Block::Text { text, .. } => Some(text.as_str()),
                            _ => None,
                        })
                        .collect();
                    let text = novacode_types::typed_prompt(&text.join("\n")).into_owned();
                    if !text.trim().is_empty() {
                        self.update(acp::SessionUpdate::UserMessageChunk(acp::ContentChunk::new(
                            text.into(),
                        )));
                    }
                }
                Message::Assistant(am) => {
                    for b in &am.content {
                        match b {
                            Block::Text { text, .. } => self.update(acp::SessionUpdate::AgentMessageChunk(
                                acp::ContentChunk::new(text.clone().into()),
                            )),
                            Block::Thinking { text, .. } => self.update(acp::SessionUpdate::AgentThoughtChunk(
                                acp::ContentChunk::new(text.clone().into()),
                            )),
                            Block::ToolCall(c) => {
                                self.tool_call(&c.id, &c.name, &c.arguments, acp::ToolCallStatus::InProgress);
                                if let Some((content, is_error)) = results.get(&c.id) {
                                    self.tool_result(&c.id, content, *is_error);
                                }
                            }
                            _ => {}
                        }
                    }
                }
                _ => {}
            }
        }
    }
}

fn options(v: &View) -> Vec<acp::SessionConfigOption> {
    let s = &v.state;
    let mut models: Vec<acp::SessionConfigSelectOption> = v
        .models
        .iter()
        .map(|(value, label)| acp::SessionConfigSelectOption::new(value.clone(), label.clone()))
        .collect();
    if !v.models.iter().any(|(m, _)| *m == s.model) {
        models.insert(0, acp::SessionConfigSelectOption::new(s.model.clone(), s.model.clone()));
    }
    let levels: Vec<_> = ThinkingLevel::ALL
        .iter()
        .map(|l| acp::SessionConfigSelectOption::new(l.as_str(), l.as_str()))
        .collect();
    let mut out = vec![
        acp::SessionConfigOption::select("model", "Model", s.model.clone(), models)
            .category(acp::SessionConfigOptionCategory::Model),
        acp::SessionConfigOption::select("thought_level", "Thinking", s.level.as_str(), levels)
            .category(acp::SessionConfigOptionCategory::ThoughtLevel),
    ];
    if !s.modes.is_empty() {
        let modes: Vec<_> = std::iter::once("off".to_string())
            .chain(s.modes.iter().cloned())
            .map(|m| acp::SessionConfigSelectOption::new(m.clone(), m))
            .collect();
        let cur = s.mode.clone().unwrap_or_else(|| "off".into());
        out.push(
            acp::SessionConfigOption::select("mode", "Mode", cur, modes)
                .category(acp::SessionConfigOptionCategory::Mode),
        );
    }
    out
}

/// This folder's saved sessions, newest first.
fn list(paths: &Paths, cwd: &Path) -> Vec<acp::SessionInfo> {
    novacode_core::session::list(paths, cwd)
        .iter()
        .filter_map(|p| novacode_core::session::summary(p))
        .filter(|s| s.prompts > 0 && !s.title.starts_with(novacode_core::session::SUBAGENT_TASK_PREFIX))
        // shortcut: no paging; the newest 200 are enough for a history list.
        .take(200)
        .map(|s| {
            let ms = s
                .modified
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or_default();
            let at = format!("{}:00Z", novacode_core::memory::utc_minute(ms).replace(' ', "T"));
            acp::SessionInfo::new(s.id.clone(), cwd.to_path_buf())
                .title(s.title.clone())
                .updated_at(at)
        })
        .collect()
}

/// The prompt as novacode text: file mentions become paths, pasted images become
/// `@<temp file>` mentions that the prompt's attachment step turns into images.
fn prompt_text(blocks: &[acp::ContentBlock]) -> std::io::Result<String> {
    let mut text = String::new();
    let mut push = |s: &str| {
        if !text.is_empty() && !text.ends_with(char::is_whitespace) && !s.starts_with(char::is_whitespace) {
            text.push(' ');
        }
        text.push_str(s);
    };
    for b in blocks {
        match b {
            acp::ContentBlock::Text(t) => push(&t.text),
            acp::ContentBlock::ResourceLink(l) => push(&uri_path(&l.uri)),
            acp::ContentBlock::Resource(r) => {
                if let acp::EmbeddedResourceResource::TextResourceContents(t) = &r.resource {
                    push(&uri_path(&t.uri));
                }
            }
            acp::ContentBlock::Image(i) => push(&format!("@{}", save_image(&i.data, &i.mime_type)?.display())),
            _ => {}
        }
    }
    Ok(text)
}

fn uri_path(uri: &str) -> String {
    match uri.strip_prefix("file://") {
        Some(p) => crate::login::percent_decode(p),
        None => uri.to_string(),
    }
}

fn save_image(data: &str, mime: &str) -> std::io::Result<PathBuf> {
    static N: AtomicU64 = AtomicU64::new(0);
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data.trim())
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, format!("pasted image: {e}")))?;
    let ext = mime.strip_prefix("image/").unwrap_or("png").replace("jpeg", "jpg");
    let n = N.fetch_add(1, Ordering::Relaxed);
    // Same place and naming as a TUI paste (term::paste), so it is attached the same way.
    let path = std::env::temp_dir().join(format!(
        "novacode-clip-{}-{}-{n}.{ext}",
        std::process::id(),
        novacode_core::session::now_ms()
    ));
    std::fs::write(&path, bytes)?;
    Ok(path)
}

fn kind(tool: &str) -> acp::ToolKind {
    match tool {
        "read" => acp::ToolKind::Read,
        "edit" | "write" | "apply_patch" => acp::ToolKind::Edit,
        "bash" | "process" => acp::ToolKind::Execute,
        "grep" | "find" | "ls" | "session_search" | "tool_search" => acp::ToolKind::Search,
        "web_fetch" | "web_search" => acp::ToolKind::Fetch,
        "enter_plan_mode" | "plan_complete" => acp::ToolKind::SwitchMode,
        _ => acp::ToolKind::Other,
    }
}

/// The argument that says what a call does: a command, a path, a pattern, a query.
fn subject(args: &Value) -> &str {
    ["command", "path", "pattern", "query", "url", "task", "action"]
        .iter()
        .find_map(|k| args[k].as_str())
        .unwrap_or_default()
}

fn title(tool: &str, args: &Value) -> String {
    title_text(tool, subject(args))
}

fn title_text(tool: &str, subject: &str) -> String {
    let line = subject.lines().next().unwrap_or_default();
    let mut s: String = line.chars().take(120).collect();
    if s.len() < line.len() || subject.lines().nth(1).is_some() {
        s.push('…');
    }
    if s.is_empty() {
        tool.to_string()
    } else {
        format!("{tool}: {s}")
    }
}

fn at(cwd: &Path, p: &str) -> PathBuf {
    match p.strip_prefix("~/").zip(novacode_base::platform::home()) {
        Some((rest, home)) => home.join(rest),
        None => cwd.join(p),
    }
}

/// `edit` and `write` as diffs the editor can review.
fn diffs(tool: &str, args: &Value, cwd: &Path) -> Vec<acp::ToolCallContent> {
    let Some(path) = args["path"].as_str().map(|p| at(cwd, p)) else {
        return vec![];
    };
    match tool {
        "edit" => args["edits"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|e| {
                let d =
                    acp::Diff::new(path.clone(), e["new_text"].as_str()?).old_text(e["old_text"].as_str()?.to_string());
                Some(d.into())
            })
            .collect(),
        "write" => args["content"]
            .as_str()
            .map(|c| vec![acp::Diff::new(path, c).into()])
            .unwrap_or_default(),
        _ => vec![],
    }
}

fn locations(tool: &str, args: &Value, cwd: &Path) -> Vec<acp::ToolCallLocation> {
    if !matches!(tool, "read" | "edit" | "write" | "ls" | "grep" | "find") {
        return vec![];
    }
    let Some(p) = args["path"].as_str() else { return vec![] };
    let mut l = acp::ToolCallLocation::new(at(cwd, p));
    if let Some(line) = args["offset"].as_u64() {
        l = l.line(line as u32);
    }
    vec![l]
}

/// A tool's text result for the editor.
fn output(content: &[Block]) -> Vec<acp::ToolCallContent> {
    const MAX: usize = 20_000;
    let mut text = String::new();
    for b in content {
        if let Block::Text { text: t, .. } = b {
            text.push_str(t);
        }
    }
    if text.len() > MAX {
        let mut cut = MAX;
        while !text.is_char_boundary(cut) {
            cut -= 1;
        }
        text.truncate(cut);
        text.push_str("\n… (cut; the model saw all of it)");
    }
    if text.is_empty() {
        return vec![];
    }
    vec![format!("```\n{text}\n```").into()]
}

/// Field name of question `i` (zero-padded so the form keeps the questions' order).
fn key(i: usize) -> String {
    format!("q{i:02}")
}

fn form(questions: &[Question]) -> acp::ElicitationSchema {
    let mut s = acp::ElicitationSchema::new();
    for (i, q) in questions.iter().enumerate() {
        let title = q.header.clone().unwrap_or_else(|| q.question.clone());
        if q.options.is_empty() {
            s = s.property(
                key(i),
                acp::StringPropertySchema::new()
                    .title(title)
                    .description(q.question.clone()),
                true,
            );
            continue;
        }
        let opts: Vec<acp::EnumOption> = q
            .options
            .iter()
            .map(|c| {
                let t = if c.recommended {
                    format!("{} (recommended)", c.label)
                } else {
                    c.label.clone()
                };
                acp::EnumOption::new(c.label.clone(), t).description(c.description.clone())
            })
            .collect();
        s = if q.multi {
            s.property(
                key(i),
                acp::MultiSelectPropertySchema::titled(opts)
                    .title(title)
                    .description(q.question.clone()),
                false,
            )
        } else {
            s.property(
                key(i),
                acp::StringPropertySchema::new()
                    .title(title)
                    .description(q.question.clone())
                    .one_of(opts),
                false,
            )
        };
        s = s.property(
            format!("{}_text", key(i)),
            acp::StringPropertySchema::new().title("Other answer or note (optional)"),
            false,
        );
    }
    s
}

fn replies(questions: &[Question], content: &BTreeMap<String, acp::ElicitationContentValue>) -> Vec<Reply> {
    let text = |k: &str| match content.get(k) {
        Some(acp::ElicitationContentValue::String(s)) if !s.trim().is_empty() => Some(s.clone()),
        _ => None,
    };
    questions
        .iter()
        .enumerate()
        .map(|(i, q)| {
            let k = key(i);
            let (chosen, text) = if q.options.is_empty() {
                (vec![], text(&k))
            } else {
                let chosen = match content.get(&k) {
                    Some(acp::ElicitationContentValue::String(s)) => vec![s.clone()],
                    Some(acp::ElicitationContentValue::StringArray(v)) => v.clone(),
                    _ => vec![],
                };
                (chosen, text(&format!("{k}_text")))
            };
            Reply {
                question: q.question.clone(),
                chosen,
                text,
                preview: None,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn q(v: Value) -> Question {
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn form_answers_become_replies() {
        let qs = [
            q(json!({"question": "Which?", "options": [{"label": "A"}, {"label": "B"}], "multi": true})),
            q(json!({"question": "Name?"})),
        ];
        let schema = to_value(form(&qs));
        assert_eq!(schema["properties"]["q00"]["type"], "array");
        assert_eq!(schema["required"], json!(["q01"]));
        let content: BTreeMap<String, acp::ElicitationContentValue> =
            serde_json::from_value(json!({"q00": ["A", "B"], "q00_text": " ", "q01": "nova"})).unwrap();
        let r = replies(&qs, &content);
        assert_eq!(r[0].chosen, ["A", "B"]);
        assert_eq!(r[0].text, None, "a blank note is no note");
        assert_eq!((r[1].chosen.len(), r[1].text.as_deref()), (0, Some("nova")));
    }

    #[test]
    fn prompt_blocks_become_text() {
        let blocks: Vec<acp::ContentBlock> = serde_json::from_value(json!([
            {"type": "text", "text": "look at"},
            {"type": "resource_link", "name": "a b.rs", "uri": "file:///w/a%20b.rs"},
            {"type": "text", "text": " please"}
        ]))
        .unwrap();
        assert_eq!(prompt_text(&blocks).unwrap(), "look at /w/a b.rs please");
    }

    #[test]
    fn edits_show_as_diffs_and_titles_stay_short() {
        let args = json!({"path": "src/x.rs", "edits": [{"old_text": "a", "new_text": "b"}]});
        let d = to_value(diffs("edit", &args, Path::new("/w")));
        assert_eq!(d[0]["type"], "diff");
        assert_eq!(
            (d[0]["path"].as_str(), d[0]["oldText"].as_str()),
            (Some("/w/src/x.rs"), Some("a"))
        );
        assert_eq!(title("bash", &json!({"command": "ls\nrm x"})), "bash: ls…");
        assert_eq!(title("todo", &json!({})), "todo");
    }
}
