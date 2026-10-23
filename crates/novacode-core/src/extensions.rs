//! Script extensions (L7.2 tier 1): discovery, loading with a stripped-source
//! cache, tools, commands, hooks and event handlers. A crashing or hung
//! extension is disabled with a notice; the session carries on.

use crate::hooks::{Gate, Handler};
use futures_util::future::BoxFuture;
use novacode_script::ext::{Backend, ExtHost, Extension, Failure};
use novacode_tools::{Exposure, Tool, ToolCtx, ToolOutput};
use novacode_types::{AgentEvent, AssistantMessage, Block, Message, StreamEvent};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use tokio::sync::broadcast;

const HOOK_TIMEOUT: Duration = Duration::from_secs(10);
const TOOL_TIMEOUT: Duration = Duration::from_secs(300);
/// Consecutive thrown errors before an extension is turned off.
const MAX_THROWS: u32 = 3;

/// Extension files: `<dir>/<name>.{ts,js,mjs}` or `<dir>/<name>/index.{ts,js}`, sorted by name.
pub fn discover(dirs: &[PathBuf]) -> Vec<(String, PathBuf)> {
    let mut out: Vec<(String, PathBuf)> = Vec::new();
    for dir in dirs {
        let Ok(rd) = std::fs::read_dir(dir) else { continue };
        let mut found: Vec<(String, PathBuf)> = rd
            .flatten()
            .filter_map(|e| {
                let p = e.path();
                let name = p.file_stem()?.to_str()?.to_string();
                if name.starts_with('.') {
                    return None;
                }
                if p.is_dir() {
                    ["index.ts", "index.js"]
                        .iter()
                        .map(|f| p.join(f))
                        .find(|f| f.is_file())
                        .map(|f| (name, f))
                } else {
                    matches!(p.extension()?.to_str()?, "ts" | "js" | "mjs").then_some((name, p))
                }
            })
            .collect();
        found.sort();
        for (n, p) in found {
            // The earlier directory (project) wins a name clash.
            if !out.iter().any(|(m, _)| *m == n) {
                out.push((n, p));
            }
        }
    }
    out
}

/// The host side of the `novacode` object (API v1); process extensions reach it through `host` requests.
pub struct Host {
    name: String,
    cwd: PathBuf,
    home: PathBuf,
    sink: Arc<Sink>,
    /// The merged settings, for `settings.get`.
    settings: Arc<Value>,
}

impl Host {
    pub fn sink(&self) -> Arc<Sink> {
        self.sink.clone()
    }
    pub fn home(&self) -> &Path {
        &self.home
    }
    /// `extensions.<name>` of the settings: an extension's own block.
    pub fn settings_of(&self, name: &str) -> Value {
        self.settings["extensions"][name].clone()
    }
    pub fn new(name: &str, cwd: &Path, home: &Path, sink: Arc<Sink>, settings: Arc<Value>) -> Self {
        Host {
            name: name.into(),
            cwd: cwd.into(),
            home: home.into(),
            sink,
            settings,
        }
    }
}

/// A saved reminder for the model: (extension name, text, wake an idle session).
pub type Reminder = Arc<dyn Fn(&str, String, bool) + Send + Sync>;

/// What extensions may read and call in their session (API v1, L7.11), set by the agent.
pub struct SessionApi {
    /// The conversation as of the last refresh, system prompt left out.
    pub messages: Arc<dyn Fn() -> Arc<Vec<Message>> + Send + Sync>,
    /// One model call on the session's provider and login: (system, messages, max tokens).
    #[allow(clippy::type_complexity)]
    pub complete: Arc<
        dyn Fn(Option<String>, Vec<Message>, Option<u32>) -> BoxFuture<'static, Result<AssistantMessage, String>>
            + Send
            + Sync,
    >,
    /// A tool call through the session's pipeline (rules, approvals, hooks).
    pub call_tool: Arc<dyn Fn(String, Value) -> BoxFuture<'static, ToolOutput> + Send + Sync>,
    /// Save `(extension, kind, data)` in the session file.
    #[allow(clippy::type_complexity)]
    pub state_append: Arc<dyn Fn(&str, &str, Value) + Send + Sync>,
    /// The data an extension saved under `kind`, oldest first, on the current branch.
    #[allow(clippy::type_complexity)]
    pub state_entries: Arc<dyn Fn(&str, &str) -> Vec<Value> + Send + Sync>,
    /// A user message: `steer` into the running turn, else after it (or now when idle).
    pub send: Arc<dyn Fn(String, bool) + Send + Sync>,
}

/// Where extensions reach their session, filled in when it attaches.
#[derive(Default)]
pub struct Sink {
    pub session: OnceLock<SessionApi>,
    pub events: OnceLock<broadcast::Sender<AgentEvent>>,
    pub remind: OnceLock<Reminder>,
    /// Who answers `nc.ui.ask/select/confirm/input`: the session's windows (L5.21 form).
    pub asker: OnceLock<Arc<dyn novacode_tools::ask::Asker>>,
    /// UI ops sent before the session attached (an extension's setup); the hub
    /// starts its UI log from them, since no window was listening yet.
    early: std::sync::Mutex<Vec<AgentEvent>>,
}

impl Sink {
    /// A UI op from extension `name` (L9 UI API). The JSON is checked here, at the
    /// boundary, so a bad op is the extension's error, never a window's.
    pub fn ui(&self, name: &str, op: Value) -> Result<(), String> {
        let op: novacode_types::ui::UiOp = serde_json::from_value(op).map_err(|e| format!("ui: {e}"))?;
        let ev = AgentEvent::Ui {
            ext: name.to_string(),
            op,
        };
        match self.events.get() {
            Some(tx) => {
                let _ = tx.send(ev);
            }
            None => self.early.lock().unwrap().push(ev),
        }
        Ok(())
    }

    /// `remind(text, wake)` from an extension: stored as a hidden `<system-reminder>`
    /// at the next model call, appended, so the cache holds (L4.12).
    pub fn remind(&self, name: &str, text: String, wake: bool) -> Result<(), String> {
        if text.trim().is_empty() {
            return Err("remind: text is empty".into());
        }
        let f = self.remind.get().ok_or("remind: no session yet")?;
        f(name, text, wake);
        Ok(())
    }
}

fn notice(events: &OnceLock<broadcast::Sender<AgentEvent>>, text: String) {
    match events.get() {
        Some(tx) => {
            let _ = tx.send(AgentEvent::Stream(StreamEvent::Notice { text }));
        }
        None => tracing::warn!("{text}"),
    }
}

impl ExtHost for Host {
    fn call(&self, op: String, args: Value) -> BoxFuture<'static, Result<Value, String>> {
        let (name, cwd, home, sink, settings) = (
            self.name.clone(),
            self.cwd.clone(),
            self.home.clone(),
            self.sink.clone(),
            self.settings.clone(),
        );
        Box::pin(async move {
            let path = |p: &Value| -> Result<PathBuf, String> {
                let p = p.as_str().ok_or("path is required")?;
                Ok(match p.strip_prefix("~/") {
                    Some(r) => home.join(r),
                    None => cwd.join(p),
                })
            };
            match op.as_str() {
                "exec" => {
                    let cmd = args["command"].as_str().ok_or("command is required")?.to_string();
                    // Extensions are code the user installed; only the never-allowed targets are refused.
                    let risk = crate::risk::classify(
                        &cmd,
                        &crate::risk::Ctx {
                            cwd: &cwd,
                            home: &home,
                            protected: &[],
                        },
                    );
                    if risk.level == crate::risk::Level::Catastrophic {
                        return Err(format!("refused: {}", risk.reason));
                    }
                    let secs = args["timeout"].as_u64().unwrap_or(60).clamp(1, 600);
                    let run = tokio::process::Command::new("bash")
                        .arg("-c")
                        .arg(&cmd)
                        .current_dir(&cwd)
                        .stdin(std::process::Stdio::null())
                        .env(crate::hooks::DISABLED_ENV, "1")
                        .kill_on_drop(true)
                        .output();
                    let o = tokio::time::timeout(Duration::from_secs(secs), run)
                        .await
                        .map_err(|_| format!("timed out after {secs}s"))?
                        .map_err(|e| e.to_string())?;
                    Ok(json!({
                        "code": o.status.code(),
                        "stdout": String::from_utf8_lossy(&o.stdout),
                        "stderr": String::from_utf8_lossy(&o.stderr),
                    }))
                }
                "readFile" => std::fs::read_to_string(path(&args["path"])?)
                    .map(Value::String)
                    .map_err(|e| e.to_string()),
                "writeFile" => {
                    let p = path(&args["path"])?;
                    if let Some(d) = p.parent() {
                        std::fs::create_dir_all(d).map_err(|e| e.to_string())?;
                    }
                    std::fs::write(p, args["text"].as_str().unwrap_or_default())
                        .map(|_| Value::Null)
                        .map_err(|e| e.to_string())
                }
                "notify" => {
                    notice(
                        &sink.events,
                        format!("[{name}] {}", args["text"].as_str().unwrap_or_default()),
                    );
                    Ok(Value::Null)
                }
                "ui" => sink.ui(&name, args).map(|_| Value::Null),
                "ask" => {
                    let qs: Vec<novacode_types::Question> =
                        serde_json::from_value(args["questions"].clone()).map_err(|e| format!("ask: {e}"))?;
                    let a = sink
                        .asker
                        .get()
                        .cloned()
                        .ok_or("ask: this session has no window to ask in")?;
                    Ok(serde_json::to_value(a.ask(qs).await).unwrap_or_default())
                }
                "remind" => sink
                    .remind(
                        &name,
                        args["text"].as_str().unwrap_or_default().to_string(),
                        args["wake"] == true,
                    )
                    .map(|_| Value::Null),
                "log" => {
                    tracing::info!(extension = %name, "{}", args["text"].as_str().unwrap_or_default());
                    Ok(Value::Null)
                }
                "messages" => {
                    let api = sink.session.get().ok_or("session.messages: no session yet")?;
                    Ok(serde_json::to_value(&*(api.messages)()).unwrap_or_default())
                }
                "complete" => {
                    let api = sink.session.get().ok_or("complete: no session yet")?;
                    let messages = args["messages"]
                        .as_array()
                        .ok_or("complete: messages is required")?
                        .iter()
                        .map(|m| {
                            let text = m["content"]
                                .as_str()
                                .ok_or("complete: each message needs text content")?;
                            match m["role"].as_str() {
                                Some("user") => Ok(Message::user(text)),
                                Some("assistant") => Ok(Message::Assistant(AssistantMessage {
                                    content: vec![Block::text(text)],
                                    stop: novacode_types::StopReason::Stop,
                                    usage: Default::default(),
                                    provider: String::new(),
                                    model: String::new(),
                                    response_id: None,
                                    error_message: None,
                                    thinking_level: None,
                                })),
                                _ => Err("complete: role must be user or assistant"),
                            }
                        })
                        .collect::<Result<Vec<_>, _>>()?;
                    let system = args["system"].as_str().map(String::from);
                    let max = args["maxTokens"].as_u64().map(|n| n.min(u32::MAX as u64) as u32);
                    let m = (api.complete)(system, messages, max).await?;
                    Ok(json!({ "text": m.text(), "usage": m.usage, "model": m.model }))
                }
                "spawnAgent" => {
                    let api = sink.session.get().ok_or("spawnAgent: no session yet")?;
                    let task = args["task"].as_str().ok_or("spawnAgent: task is required")?;
                    let mut a = match &args["opts"] {
                        Value::Object(o) => o.clone(),
                        _ => Default::default(),
                    };
                    a.insert("action".into(), json!("spawn"));
                    a.insert("task".into(), json!(task));
                    // The Subagents panel shows which extension started it.
                    let title = a.get("title").and_then(Value::as_str).unwrap_or(task);
                    let title = format!("[ext:{name}] {}", title.chars().take(50).collect::<String>());
                    a.insert("title".into(), json!(title));
                    let out = (api.call_tool)("agent".into(), Value::Object(a)).await;
                    let id = out.details.as_ref().and_then(|d| d["id"].as_str()).map(String::from);
                    let Some(id) = id.filter(|_| !out.is_error) else {
                        return Err(text_of(&out));
                    };
                    // shortcut: no wait; one handler runs at a time per extension, so waiting
                    // would freeze it (and its wall-clock timeout would disable it). Reports
                    // arrive through the agent tool's `notify`.
                    Ok(json!({ "id": id }))
                }
                "settings" => {
                    let mut v = &*settings;
                    for k in args["path"]
                        .as_str()
                        .unwrap_or_default()
                        .split('.')
                        .filter(|k| !k.is_empty())
                    {
                        v = &v[k];
                    }
                    Ok(v.clone())
                }
                "stateAppend" => {
                    let api = sink.session.get().ok_or("state.append: no session yet")?;
                    let kind = args["kind"]
                        .as_str()
                        .filter(|k| !k.is_empty())
                        .ok_or("state.append: kind is required")?;
                    (api.state_append)(&name, kind, args["data"].clone());
                    Ok(Value::Null)
                }
                "stateEntries" => {
                    let api = sink.session.get().ok_or("state.entries: no session yet")?;
                    Ok(Value::Array((api.state_entries)(
                        &name,
                        args["kind"].as_str().unwrap_or_default(),
                    )))
                }
                "sendMessage" => {
                    let api = sink.session.get().ok_or("sendMessage: no session yet")?;
                    let text = args["text"]
                        .as_str()
                        .filter(|t| !t.trim().is_empty())
                        .ok_or("sendMessage: text is empty")?;
                    let steer = match args["deliverAs"].as_str().unwrap_or("followUp") {
                        "steer" => true,
                        "followUp" => false,
                        d => return Err(format!("sendMessage: deliverAs `{d}` is not steer or followUp")),
                    };
                    (api.send)(text.to_string(), steer);
                    Ok(Value::Null)
                }
                _ => Err(format!("unknown operation `{op}`")),
            }
        })
    }
}

/// One loaded extension plus its health.
pub struct Loaded {
    pub ext: Arc<dyn Backend>,
    pub path: PathBuf,
    off: AtomicBool,
    throws: AtomicU32,
}

/// Every extension of a session.
pub struct Extensions {
    pub loaded: Vec<Arc<Loaded>>,
    /// (name, error) of extensions that failed to load.
    pub failed: Vec<(String, String)>,
    sink: Arc<Sink>,
    cwd: PathBuf,
}

impl Extensions {
    pub async fn load(dirs: &[PathBuf], cwd: &Path, home: &Path, cache: &Path) -> Arc<Extensions> {
        Self::load_all(dirs, &[], cwd, home, cache, "", Path::new(""), Value::Null).await
    }

    /// Script extensions from `dirs`, then process extensions (`(name, config)`, L7.2 tier 2).
    #[allow(clippy::too_many_arguments)]
    pub async fn load_all(
        dirs: &[PathBuf],
        procs: &[(String, crate::process_ext::Config)],
        cwd: &Path,
        home: &Path,
        cache: &Path,
        session: &str,
        log_dir: &Path,
        settings: Value,
    ) -> Arc<Extensions> {
        let sink: Arc<Sink> = Arc::default();
        let settings = Arc::new(settings);
        let mut loaded = Vec::new();
        let mut failed = Vec::new();
        for (name, c) in procs {
            let log = log_dir.join(format!("ext-{name}.log"));
            let host = Arc::new(Host::new(name, cwd, home, sink.clone(), settings.clone()));
            match crate::process_ext::ProcessExt::start(name, c, cwd, session, &log, host).await {
                Ok(ext) => loaded.push(Arc::new(Loaded {
                    ext: ext as Arc<dyn Backend>,
                    path: PathBuf::from(&c.command),
                    off: AtomicBool::new(false),
                    throws: AtomicU32::new(0),
                })),
                Err(e) => failed.push((name.clone(), e)),
            }
        }
        for (name, path) in discover(dirs) {
            let src = match std::fs::read_to_string(&path) {
                Ok(s) => s,
                Err(e) => {
                    failed.push((name, e.to_string()));
                    continue;
                }
            };
            let host = Arc::new(Host::new(&name, cwd, home, sink.clone(), settings.clone()));
            let js = novacode_script::ts::cached(&src, cache);
            match Extension::load(&name, js, host, 64 << 20, Some((path.clone(), cache.to_path_buf()))).await {
                Ok(ext) => loaded.push(Arc::new(Loaded {
                    ext,
                    path,
                    off: AtomicBool::new(false),
                    throws: AtomicU32::new(0),
                })),
                Err(e) => failed.push((name, e)),
            }
        }
        Arc::new(Extensions {
            loaded,
            failed,
            sink,
            cwd: cwd.into(),
        })
    }

    /// Route notices (disabled extensions, `novacode.notify`) to the session's events.
    pub fn attach(&self, tx: broadcast::Sender<AgentEvent>, remind: Reminder) {
        let _ = self.sink.events.set(tx);
        let _ = self.sink.remind.set(remind);
        for (n, e) in &self.failed {
            notice(&self.sink.events, format!("extension `{n}` failed to load: {e}"));
        }
    }

    /// `session.messages()`, `complete()` and `spawnAgent()` reach this session.
    pub fn connect(&self, api: SessionApi) {
        let _ = self.sink.session.set(api);
    }

    /// Dialogs from extensions go to the session's windows.
    pub fn set_asker(&self, a: Arc<dyn novacode_tools::ask::Asker>) {
        let _ = self.sink.asker.set(a);
    }

    /// UI ops the extensions sent during setup (see [`Sink`]).
    pub fn setup_ui(&self) -> Vec<AgentEvent> {
        self.sink.early.lock().unwrap().clone()
    }

    pub fn is_empty(&self) -> bool {
        self.loaded.is_empty()
    }

    /// Run one handler, turning crashes into a disabled extension and a notice.
    async fn invoke(&self, l: &Loaded, handler: u32, payload: &Value, timeout: Duration) -> Result<Value, String> {
        if l.off.load(Ordering::Relaxed) {
            return Err(format!("extension `{}` is disabled", l.ext.name()));
        }
        let timeout = match l.ext.hook_timeout() {
            Some(t) if timeout == HOOK_TIMEOUT => t,
            _ => timeout,
        };
        match l.ext.invoke(handler, payload, timeout).await {
            Ok(v) => {
                l.throws.store(0, Ordering::Relaxed);
                Ok(v)
            }
            Err(Failure::Threw(e)) => {
                if l.throws.fetch_add(1, Ordering::Relaxed) + 1 >= MAX_THROWS {
                    self.disable(l, &format!("{MAX_THROWS} errors in a row; last: {e}"));
                }
                Err(e)
            }
            Err(Failure::Fatal(e)) => {
                self.disable(l, &e);
                Err(e)
            }
        }
    }

    fn disable(&self, l: &Loaded, why: &str) {
        if !l.off.swap(true, Ordering::Relaxed) {
            notice(
                &self.sink.events,
                format!("extension `{}` disabled: {why} (fix it and /reload)", l.ext.name()),
            );
        }
    }

    pub fn tools(self: &Arc<Self>) -> Vec<Arc<dyn Tool>> {
        let mut out: Vec<Arc<dyn Tool>> = Vec::new();
        for l in &self.loaded {
            for t in &l.ext.reg().tools {
                let exposure = t
                    .exposure
                    .as_ref()
                    .and_then(|e| serde_json::from_value(Value::String(e.clone())).ok())
                    .unwrap_or(Exposure::Listed);
                out.push(Arc::new(ScriptTool {
                    exts: self.clone(),
                    owner: l.clone(),
                    reg: t.clone(),
                    exposure,
                }));
            }
        }
        out
    }

    /// Slash commands `(/name, description)` for completion.
    pub fn commands(&self) -> Vec<(String, String)> {
        self.loaded
            .iter()
            .flat_map(|l| {
                l.ext
                    .reg()
                    .commands
                    .iter()
                    .map(|c| (format!("/{}", c.name), c.description.clone()))
            })
            .collect()
    }

    /// Run `/name args` if an extension owns it: `Some(Ok(text to show))`.
    pub async fn command(&self, text: &str) -> Option<Result<String, String>> {
        let t = text.trim().strip_prefix('/')?;
        let (name, args) = t
            .split_once(char::is_whitespace)
            .map_or((t, ""), |(a, b)| (a, b.trim()));
        let (l, c) = self
            .loaded
            .iter()
            .find_map(|l| l.ext.reg().commands.iter().find(|c| c.name == name).map(|c| (l, c)))?;
        Some(
            self.invoke(l, c.handler, &json!(args), TOOL_TIMEOUT)
                .await
                .map(|v| match v {
                    Value::Null => String::new(),
                    Value::String(s) => s,
                    v => v.to_string(),
                }),
        )
    }

    fn handlers<'a>(&'a self, event: &'a str) -> impl Iterator<Item = (&'a Arc<Loaded>, u32)> + 'a {
        self.loaded.iter().flat_map(move |l| {
            l.ext
                .reg()
                .events
                .iter()
                .filter(move |(e, _)| e == event)
                .map(move |(_, h)| (l, *h))
        })
    }

    /// Forward session events to `on(type, …)` handlers (observe-only), until the session ends.
    /// `input` handlers, in load order, before a prompt or steer is saved (so what's
    /// stored is what's sent: the cache is safe). Each sees the previous one's text.
    /// A handler returns `{text}` to rewrite it, `{handled: true}` to swallow it (it
    /// handled the input itself), or nothing. pi's `{action: …}` form works too.
    /// `None` = handled. Fails open: a broken handler leaves the text as it was.
    pub async fn input(&self, text: &str, steer: bool) -> Option<String> {
        let mut text = text.to_string();
        for (l, h) in self.handlers("input").collect::<Vec<_>>() {
            let payload = json!({ "text": text, "source": if steer { "steer" } else { "prompt" } });
            let Ok(r) = self.invoke(l, h, &payload, HOOK_TIMEOUT).await else {
                continue;
            };
            if r["handled"] == true || r["action"] == "handled" {
                return None;
            }
            if let Some(t) = r["text"].as_str() {
                text = t.to_string();
            }
        }
        Some(text)
    }

    /// `before_request` handlers (tail-only, L3.5): each may return text (a string, a
    /// list, or `{append}`) to send after the cached prefix of this one request. It's
    /// never stored, so early history can't change and the cache holds. Fails open.
    pub async fn before_request(
        &self,
        provider: &str,
        model: &str,
        context_tokens: u64,
        last_user: Option<String>,
    ) -> Vec<String> {
        let payload = &json!({
            "provider": provider,
            "model": model,
            "cwd": self.cwd,
            "contextTokens": context_tokens,
            "lastUser": last_user,
        });
        let mut out = Vec::new();
        for (l, h) in self.handlers("before_request").collect::<Vec<_>>() {
            let Ok(r) = self.invoke(l, h, payload, HOOK_TIMEOUT).await else {
                continue;
            };
            let r = if r.is_object() { r["append"].clone() } else { r };
            match r {
                Value::String(s) if !s.trim().is_empty() => out.push(s),
                Value::Array(a) => out.extend(
                    a.into_iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .filter(|s| !s.trim().is_empty()),
                ),
                _ => {}
            }
        }
        out
    }

    /// A key or panel action for extension `ext`: its `ui_key` handlers get
    /// `{tab, node, key, width, height}` (L9 UI API). Errors only count toward disabling.
    pub async fn ui_key(&self, ext: &str, payload: Value) {
        for (l, h) in self
            .handlers("ui_key")
            .filter(|(l, _)| l.ext.name() == ext)
            .collect::<Vec<_>>()
        {
            let _ = self.invoke(l, h, &payload, HOOK_TIMEOUT).await;
        }
    }

    pub fn spawn_events(self: &Arc<Self>, mut rx: broadcast::Receiver<AgentEvent>) {
        let me = self.clone();
        let wanted: Vec<String> = me
            .loaded
            .iter()
            .flat_map(|l| l.ext.reg().events.iter().map(|(e, _)| e.clone()))
            .collect();
        if wanted.iter().all(|e| {
            matches!(
                e.as_str(),
                "before_tool" | "after_tool" | "input" | "before_request" | "ui_key"
            )
        }) {
            return;
        }
        tokio::spawn(async move {
            loop {
                let ev = match rx.recv().await {
                    Ok(ev) => ev,
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(_) => break,
                };
                let v = serde_json::to_value(&ev).unwrap_or_default();
                let name = v["type"].as_str().unwrap_or_default().to_string();
                if name == "stream" || !wanted.contains(&name) {
                    continue;
                }
                for (l, h) in me.handlers(&name).collect::<Vec<_>>() {
                    let _ = me.invoke(l, h, &v, HOOK_TIMEOUT).await;
                }
            }
        });
    }
}

impl Handler for Extensions {
    fn before_tool<'a>(&'a self, tool: &'a str, args: &'a Value) -> BoxFuture<'a, Gate> {
        Box::pin(async move {
            let mut args = args.clone();
            let mut changed = false;
            for (l, h) in self.handlers("before_tool").collect::<Vec<_>>() {
                // Fail open: a broken handler never blocks the call.
                let Ok(r) = self
                    .invoke(
                        l,
                        h,
                        &json!({ "tool": tool, "args": args, "cwd": self.cwd }),
                        HOOK_TIMEOUT,
                    )
                    .await
                else {
                    continue;
                };
                if let Some(why) = r["block"].as_str() {
                    return Gate::Block(format!("{} ({})", why, l.ext.name()));
                }
                if r["args"].is_object() {
                    args = r["args"].clone();
                    changed = true;
                }
            }
            if changed { Gate::Args(args) } else { Gate::Allow }
        })
    }

    fn wants_touched(&self) -> bool {
        self.handlers("after_tool").next().is_some()
    }

    fn after_tool<'a>(
        &'a self,
        tool: &'a str,
        args: &'a Value,
        out: &'a ToolOutput,
        run: &'a crate::hooks::Run,
    ) -> BoxFuture<'a, Option<crate::hooks::After>> {
        Box::pin(async move {
            let handlers: Vec<_> = self.handlers("after_tool").collect();
            if handlers.is_empty() {
                return None;
            }
            let text = text_of(out);
            let mut payload = json!({ "tool": tool, "args": args, "isError": out.is_error, "output": text.chars().take(16_000).collect::<String>() });
            if let (Value::Object(p), Value::Object(r)) = (&mut payload, run.json()) {
                p.extend(r);
            }
            // shortcut: every handler sees the original result; the last replace/isError wins. Chain them if two extensions ever need to.
            let mut all = crate::hooks::After::default();
            let mut fb = Vec::new();
            for (l, h) in handlers {
                let Ok(v) = self.invoke(l, h, &payload, HOOK_TIMEOUT).await else {
                    continue;
                };
                if let Some(a) = crate::hooks::After::from_json(&v) {
                    fb.extend(a.append);
                    all.replace = a.replace.or(all.replace);
                    all.is_error = a.is_error.or(all.is_error);
                }
            }
            all.append = (!fb.is_empty()).then(|| fb.join("\n"));
            (all != crate::hooks::After::default()).then_some(all)
        })
    }
}

fn text_of(out: &ToolOutput) -> String {
    out.content
        .iter()
        .filter_map(|b| match b {
            Block::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

struct ScriptTool {
    exts: Arc<Extensions>,
    owner: Arc<Loaded>,
    reg: novacode_script::ext::ToolReg,
    exposure: Exposure,
}

impl Tool for ScriptTool {
    fn name(&self) -> &str {
        &self.reg.name
    }
    fn description(&self) -> &str {
        &self.reg.description
    }
    fn schema(&self) -> Value {
        if self.reg.schema.is_object() {
            self.reg.schema.clone()
        } else {
            json!({ "type": "object", "properties": {} })
        }
    }
    fn exposure(&self) -> Exposure {
        self.exposure
    }
    fn read_only(&self) -> bool {
        self.reg.read_only
    }
    fn execute<'a>(&'a self, args: Value, ctx: &'a ToolCtx) -> BoxFuture<'a, ToolOutput> {
        Box::pin(async move {
            let payload = json!({ "args": args, "cwd": ctx.cwd, "session": ctx.session_id });
            let r = tokio::select! {
                r = self.exts.invoke(&self.owner, self.reg.handler, &payload, TOOL_TIMEOUT) => r,
                _ = ctx.cancel.cancelled() => return ToolOutput::error("cancelled"),
            };
            match r {
                Err(e) => ToolOutput::error(format!("{} failed: {e}", self.reg.name)),
                Ok(Value::String(s)) => ToolOutput::text(s),
                Ok(v) if v["content"].is_string() => ToolOutput {
                    content: vec![Block::text(v["content"].as_str().unwrap_or_default().to_string())],
                    is_error: v["isError"].as_bool().unwrap_or(false),
                    details: v.get("details").cloned(),
                },
                Ok(Value::Null) => ToolOutput::text("(no output)"),
                Ok(v) => ToolOutput::text(serde_json::to_string_pretty(&v).unwrap_or_default()),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(flavor = "multi_thread")]
    async fn ui_ops_are_checked_and_buffered_until_attach() {
        let d = tempfile::tempdir().unwrap();
        let ext = d.path().join("ext");
        std::fs::create_dir_all(&ext).unwrap();
        std::fs::write(
            ext.join("ui.js"),
            "export default async (nc) => {
               await nc.ui.setStatus('k', 'hello', { style: 'accent' });
               await nc.ui.publish('t', 's', { id: 'a', label: 'A', status: 'running' });
               nc.registerTool({ name: 'bad', execute: async () => { try { await nc.ui.publish('t', 's', { nope: 1 }); return 'no error'; } catch (e) { return String(e.message); } } });
               nc.on('ui_key', (e) => nc.ui.setStatus('k', [{ text: 'key ' }, { text: e.key, style: 'warn' }]));
               nc.registerTool({ name: 'pick', execute: async () => `${await nc.ui.select('Colour?', ['red', 'blue'])} / ${await nc.ui.confirm('Sure?')}` });
             };",
        )
        .unwrap();
        let exts = Extensions::load(std::slice::from_ref(&ext), d.path(), d.path(), &d.path().join("cache")).await;
        assert!(exts.failed.is_empty(), "{:?}", exts.failed);
        let (tx, mut rx) = broadcast::channel(16);
        exts.attach(tx, Arc::new(|_: &str, _: String, _: bool| {}));
        let ops: Vec<String> = exts
            .setup_ui()
            .into_iter()
            .map(|e| match e {
                AgentEvent::Ui { ext, op } => format!("{ext}:{}", op.slot(&ext)),
                e => panic!("{e:?}"),
            })
            .collect();
        assert_eq!(ops, ["ui:status:ui:k", "ui:node:t:s:a"], "setup ops are kept, in order");
        let bad = exts.tools().into_iter().find(|t| t.name() == "bad").unwrap();
        let ctx = ToolCtx {
            cwd: d.path().into(),
            env: Arc::new(novacode_tools::LocalEnv),
            cancel: Default::default(),
            updates: None,
            files: Default::default(),
            session_id: "s".into(),
            processes: None,
            native_tools: false,
            nested: None,
            script: false,
        };
        let out = bad.execute(json!({}), &ctx).await;
        assert!(text_of(&out).contains("missing field"), "{}", text_of(&out));
        struct Pick;
        impl novacode_tools::ask::Asker for Pick {
            fn ask(&self, q: Vec<novacode_types::Question>) -> BoxFuture<'_, Option<Vec<novacode_types::Reply>>> {
                Box::pin(async move {
                    Some(vec![novacode_types::Reply {
                        question: q[0].question.clone(),
                        chosen: vec![q[0].options[1].label.clone()],
                        text: None,
                        preview: None,
                    }])
                })
            }
        }
        exts.set_asker(Arc::new(Pick));
        let pick = exts.tools().into_iter().find(|t| t.name() == "pick").unwrap();
        assert_eq!(text_of(&pick.execute(json!({}), &ctx).await), "blue / false");
        exts.ui_key("ui", json!({"key": "x"})).await;
        assert!(
            matches!(rx.try_recv().unwrap(), AgentEvent::Ui { op: novacode_types::ui::UiOp::Status { text: Some(t), style: Some(st), .. }, .. } if t == "key x" && st == "warn"),
            "segments join; their style is kept"
        );
    }

    /// L7.10: relative imports inside the extension's folder, TypeScript too; nothing outside it.
    #[tokio::test(flavor = "multi_thread")]
    async fn relative_imports_stay_inside_the_folder() {
        let d = tempfile::tempdir().unwrap();
        let ext = d.path().join("ext");
        let w = |p: &str, s: &str| {
            let f = ext.join(p);
            std::fs::create_dir_all(f.parent().unwrap()).unwrap();
            std::fs::write(f, s).unwrap();
        };
        w(
            "multi/lib/util.ts",
            "export const shout = (s: string): string => s.toUpperCase();",
        );
        w("multi/lib/index.ts", "export { shout } from './util';");
        w(
            "multi/index.ts",
            "import { shout } from './lib';\nexport default (nc: any) => nc.registerCommand('shout', { handler: (a: string) => shout(a) });",
        );
        w("secret.js", "export const x = 1;");
        w(
            "escape/index.js",
            "import { x } from '../secret.js';\nexport default () => {};",
        );
        w("bare/index.js", "import _ from 'lodash';\nexport default () => {};");
        let exts = Extensions::load(std::slice::from_ref(&ext), d.path(), d.path(), &d.path().join("cache")).await;
        assert_eq!(exts.command("/shout hey").await, Some(Ok("HEY".into())));
        let failed: std::collections::BTreeMap<_, _> = exts.failed.iter().cloned().collect();
        assert!(
            failed["escape"].contains("outside the extension's folder"),
            "{failed:?}"
        );
        assert!(failed["bare"].contains("only relative imports"), "{failed:?}");
    }

    #[tokio::test]
    async fn input_handlers_rewrite_or_handle() {
        let d = tempfile::tempdir().unwrap();
        let ext = d.path().join("ext");
        std::fs::create_dir_all(&ext).unwrap();
        std::fs::write(ext.join("a.js"), "export default (nc) => nc.on('input', ({ text }) => text.startsWith('ship') ? { text: text.replace('ship', 'commit and push') } : undefined);").unwrap();
        std::fs::write(ext.join("b.js"), "export default (nc) => nc.on('input', ({ text, source }) => text === 'ping' ? { action: 'handled' } : { text: `${text} [${source}]` });").unwrap();
        std::fs::write(
            ext.join("c.js"),
            "export default (nc) => nc.on('input', () => { throw new Error('bad'); });",
        )
        .unwrap();
        let exts = Extensions::load(std::slice::from_ref(&ext), d.path(), d.path(), &d.path().join("cache")).await;
        assert_eq!(
            exts.input("ship it", false).await.as_deref(),
            Some("commit and push it [prompt]"),
            "chained, in order"
        );
        assert_eq!(exts.input("hi", true).await.as_deref(), Some("hi [steer]"));
        assert_eq!(exts.input("ping", false).await, None, "handled");
    }

    #[tokio::test]
    async fn loads_runs_and_disables() {
        let d = tempfile::tempdir().unwrap();
        let ext = d.path().join("ext");
        std::fs::create_dir_all(ext.join("multi")).unwrap();
        std::fs::write(
            ext.join("good.ts"),
            "export default (nc: any) => {\n nc.registerTool({ name: 'shout', description: 'upper', parameters: {type:'object', properties:{t:{type:'string'}}}, execute: (a, ctx) => a.t.toUpperCase() + (ctx.session === 's' ? '' : '?') });\n nc.on('before_tool', ({ tool }) => tool === 'bash' ? { block: 'no bash here' } : undefined);\n nc.registerCommand('hello', { description: 'say hi', handler: (a) => `hi ${a}` });\n}",
        )
        .unwrap();
        std::fs::write(
            ext.join("multi/index.js"),
            "novacode.registerTool({ name: 'boom', execute: () => { while (true) {} } });",
        )
        .unwrap();
        std::fs::write(ext.join("broken.js"), "this is not js").unwrap();
        let cache = d.path().join("cache");
        let exts = Extensions::load(std::slice::from_ref(&ext), d.path(), d.path(), &cache).await;
        assert_eq!(exts.loaded.len(), 2);
        assert_eq!(exts.failed[0].0, "broken");
        assert_eq!(std::fs::read_dir(&cache).unwrap().count(), 3, "stripped source cached");
        let (tx, mut rx) = broadcast::channel(16);
        exts.attach(tx, Arc::new(|_: &str, _: String, _: bool| {}));
        assert!(
            matches!(rx.recv().await.unwrap(), AgentEvent::Stream(StreamEvent::Notice { text }) if text.contains("broken"))
        );
        let tools = exts.tools();
        let ctx = ToolCtx {
            cwd: d.path().into(),
            env: Arc::new(novacode_tools::LocalEnv),
            cancel: Default::default(),
            updates: None,
            files: Default::default(),
            session_id: "s".into(),
            processes: None,
            native_tools: false,
            nested: None,
            script: false,
        };
        let shout = tools.iter().find(|t| t.name() == "shout").unwrap();
        assert_eq!(text_of(&shout.execute(json!({"t": "hey"}), &ctx).await), "HEY");
        assert!(matches!(exts.before_tool("bash", &json!({})).await, Gate::Block(w) if w.contains("no bash here")));
        assert_eq!(exts.command("/hello nova").await, Some(Ok("hi nova".into())));
        assert_eq!(exts.command("/nope").await, None);
        // A hung tool: disabled with a notice; the other extension keeps working.
        let boom = tools.iter().find(|t| t.name() == "boom").unwrap();
        let exts2 = exts.clone();
        let out = tokio::time::timeout(Duration::from_secs(20), async {
            let l = exts2.loaded.iter().find(|l| l.ext.name() == "multi").unwrap();
            exts2
                .invoke(l, l.ext.reg().tools[0].handler, &json!({}), Duration::from_millis(300))
                .await
        })
        .await
        .unwrap();
        assert!(out.is_err());
        assert!(
            matches!(rx.recv().await.unwrap(), AgentEvent::Stream(StreamEvent::Notice { text }) if text.contains("`multi` disabled"))
        );
        assert!(boom.execute(json!({}), &ctx).await.is_error);
        assert_eq!(text_of(&shout.execute(json!({"t": "ok"}), &ctx).await), "OK");
    }
}
