//! MCP client (L7.4): stdio and streamable-HTTP servers from `~/.novacode/mcp.json`
//! plus the nearest project `.mcp.json` (Claude Code format, project wins, trust-gated).
//! Servers connect in the background; their tool lists are cached so a session
//! starts with the same tools whether or not a server is up yet. A server with a
//! cached list starts only at its first call (`Idle` until then, L7.4 revision).

use futures_util::StreamExt;
use futures_util::future::BoxFuture;
use novacode_tools::{Exposure, Tool, ToolCtx, ToolOutput};
use novacode_types::Block;
use serde::Deserialize;
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{Notify, mpsc, oneshot, watch};

const PROTOCOL: &str = "2025-06-18";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ServerConfig {
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    #[serde(default)]
    pub description: Option<String>,
    /// listed | codemode-only (default) | deferred | hidden.
    #[serde(default)]
    pub exposure: Option<Exposure>,
    /// Per-tool overrides: `{ "tool": { "exposure": "listed" } }`.
    #[serde(default)]
    pub tools: BTreeMap<String, ToolOverride>,
    #[serde(default)]
    pub disabled: bool,
    /// Seconds without progress before a call fails (default 120).
    #[serde(default)]
    pub timeout: Option<u64>,
    /// OAuth client settings for servers without dynamic registration.
    #[serde(default)]
    pub oauth: crate::mcp_oauth::OAuthConfig,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct ToolOverride {
    #[serde(default)]
    pub exposure: Option<Exposure>,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct File {
    #[serde(default)]
    mcp_servers: BTreeMap<String, ServerConfig>,
}

/// Where each server came from, for `novacode mcp list`.
#[derive(Debug, Clone)]
pub struct Configured {
    pub name: String,
    pub config: ServerConfig,
    pub source: PathBuf,
}

/// `${VAR}` / `${VAR:-default}` from the environment (Claude Code's format).
pub fn expand(s: &str) -> String {
    let mut out = String::new();
    let mut rest = s;
    while let Some(i) = rest.find("${") {
        out.push_str(&rest[..i]);
        let Some(end) = rest[i..].find('}') else { break };
        let inner = &rest[i + 2..i + end];
        let (var, def) = inner.split_once(":-").unwrap_or((inner, ""));
        out.push_str(&std::env::var(var).unwrap_or_else(|_| def.to_string()));
        rest = &rest[i + end + 1..];
    }
    out.push_str(rest);
    out
}

/// The nearest `.mcp.json` from `cwd` up.
pub fn project_file(cwd: &Path) -> Option<PathBuf> {
    cwd.ancestors().map(|d| d.join(".mcp.json")).find(|p| p.is_file())
}

/// Global servers, then the project's (when trusted) replacing same-named ones.
pub fn load(home: &Path, cwd: &Path, trusted: bool) -> (Vec<Configured>, Vec<String>) {
    let mut out: BTreeMap<String, Configured> = BTreeMap::new();
    let mut warnings = Vec::new();
    // Later files win a name: packages' servers, then yours, then the project's.
    type Parsed = Result<BTreeMap<String, ServerConfig>, String>;
    let mut sources: Vec<(PathBuf, Parsed)> = Vec::new();
    for p in crate::packages::enabled(home, cwd, trusted).into_iter().rev() {
        for (f, r) in crate::plugin::Parts::of(&p).mcp() {
            let r = r.and_then(|m| serde_json::from_value(Value::Object(m)).map_err(|e| e.to_string()));
            sources.push((f, r));
        }
    }
    let mut files = vec![home.join("mcp.json")];
    if trusted && let Some(p) = project_file(cwd) {
        files.push(p);
    }
    for f in files {
        let Ok(text) = std::fs::read_to_string(&f) else {
            continue;
        };
        let r = serde_json::from_str::<File>(&text)
            .map(|f| f.mcp_servers)
            .map_err(|e| e.to_string());
        sources.push((f, r));
    }
    for (f, r) in sources {
        match r {
            Ok(servers) => {
                for (name, config) in servers {
                    if name.contains("__") || name.is_empty() {
                        warnings.push(format!(
                            "{}: MCP server name `{name}` may not contain `__`",
                            f.display()
                        ));
                        continue;
                    }
                    out.insert(
                        name.clone(),
                        Configured {
                            name,
                            config,
                            source: f.clone(),
                        },
                    );
                }
            }
            Err(e) => warnings.push(format!("{}: {e}", f.display())),
        }
    }
    (out.into_values().filter(|c| !c.config.disabled).collect(), warnings)
}

/// One MCP tool as the server described it.
#[derive(Debug, Clone, serde::Serialize, Deserialize, PartialEq)]
pub struct ToolSpec {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default, rename = "inputSchema")]
    pub input_schema: Value,
    /// `readOnlyHint` here lets the tool run in read-only mode.
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub annotations: Value,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Status {
    /// Not started: its tools come from the cache; it starts at its first call.
    Idle,
    Connecting,
    Ready,
    Failed(String),
}

type Pending = Arc<Mutex<HashMap<u64, (oneshot::Sender<Result<Value, String>>, Arc<Notify>)>>>;

enum Transport {
    Stdio {
        tx: mpsc::UnboundedSender<String>,
        pending: Pending,
        pid: Option<u32>,
    },
    Http {
        url: String,
        headers: BTreeMap<String, String>,
        session: Mutex<Option<String>>,
        http: reqwest::Client,
        /// Bearer tokens from `novacode mcp login`; unused when headers set Authorization.
        auth: Option<crate::mcp_oauth::Auth>,
        name: String,
    },
}

impl Drop for Transport {
    fn drop(&mut self) {
        if let Transport::Stdio { pid: Some(pid), .. } = self {
            novacode_tools::process::kill_group_now(*pid);
        }
    }
}

struct Conn {
    transport: Transport,
    ids: AtomicU64,
}

impl Conn {
    /// One JSON-RPC request; `progress` is notified on each progress notification.
    async fn request(&self, method: &str, params: Value, idle: Duration) -> Result<Value, String> {
        let id = self.ids.fetch_add(1, Ordering::Relaxed);
        let mut params = params;
        if let Some(o) = params.as_object_mut() {
            o.insert("_meta".into(), json!({ "progressToken": id }));
        }
        let msg = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        match &self.transport {
            Transport::Stdio { tx, pending, .. } => {
                let (otx, mut orx) = oneshot::channel();
                let progress = Arc::new(Notify::new());
                pending.lock().unwrap().insert(id, (otx, progress.clone()));
                if tx.send(msg.to_string()).is_err() {
                    pending.lock().unwrap().remove(&id);
                    return Err("the server has exited".into());
                }
                loop {
                    tokio::select! {
                        r = &mut orx => return r.unwrap_or_else(|_| Err("the server has exited".into())),
                        _ = progress.notified() => continue,
                        _ = tokio::time::sleep(idle) => {
                            pending.lock().unwrap().remove(&id);
                            let _ = tx.send(json!({"jsonrpc": "2.0", "method": "notifications/cancelled", "params": {"requestId": id, "reason": "timeout"}}).to_string());
                            return Err(format!("no answer or progress for {}s", idle.as_secs()));
                        }
                    }
                }
            }
            Transport::Http { .. } => self.http(msg, Some(id), idle).await,
        }
    }

    async fn notify(&self, method: &str) {
        let msg = json!({ "jsonrpc": "2.0", "method": method });
        match &self.transport {
            Transport::Stdio { tx, .. } => {
                let _ = tx.send(msg.to_string());
            }
            Transport::Http { .. } => {
                let _ = self.http(msg, None, Duration::from_secs(10)).await;
            }
        }
    }

    // shortcut: no GET listening stream, so server-initiated notifications over HTTP are missed.
    async fn http(&self, msg: Value, id: Option<u64>, idle: Duration) -> Result<Value, String> {
        let Transport::Http {
            url,
            headers,
            session,
            http,
            auth,
            name,
        } = &self.transport
        else {
            unreachable!()
        };
        let mut token = match auth {
            Some(a) => a.token().await?,
            None => None,
        };
        let mut retried = false;
        let resp = loop {
            let resp = self.post(url, headers, session, http, &msg, token.as_deref()).await?;
            // 401, or 403 asking for more scope: refresh once, else say how to log in.
            let insufficient = resp.status().as_u16() == 403
                && resp
                    .headers()
                    .get("www-authenticate")
                    .and_then(|v| v.to_str().ok())
                    .is_some_and(|h| h.contains("insufficient_scope"));
            if resp.status().as_u16() != 401 && !insufficient {
                break resp;
            }
            let Some(a) = auth else { break resp };
            match (&token, retried) {
                (Some(t), false) if !insufficient => {
                    retried = true;
                    match a.refresh(t).await {
                        Ok(t) => token = Some(t),
                        Err(e) => return Err(format!("{e}; run `novacode mcp login {name}`")),
                    }
                }
                _ if insufficient => {
                    return Err(format!(
                        "the server wants more permissions; run `novacode mcp login {name}` again"
                    ));
                }
                _ => return Err(format!("the server needs a login; run `novacode mcp login {name}`")),
            }
        };
        if let Some(s) = resp.headers().get("mcp-session-id").and_then(|v| v.to_str().ok()) {
            *session.lock().unwrap() = Some(s.to_string());
        }
        let status = resp.status();
        let Some(id) = id else { return Ok(Value::Null) };
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            let hint = if status.as_u16() == 401 {
                " (set an Authorization header, or run `novacode mcp login`)"
            } else {
                ""
            };
            return Err(format!(
                "HTTP {}{hint}: {}",
                status.as_u16(),
                body.chars().take(300).collect::<String>()
            ));
        }
        self.read_answer(resp, id, idle).await
    }

    async fn post(
        &self,
        url: &str,
        headers: &BTreeMap<String, String>,
        session: &Mutex<Option<String>>,
        http: &reqwest::Client,
        msg: &Value,
        token: Option<&str>,
    ) -> Result<reqwest::Response, String> {
        let mut req = http
            .post(url)
            .header("accept", "application/json, text/event-stream")
            .header("content-type", "application/json")
            .header("mcp-protocol-version", PROTOCOL)
            .body(msg.to_string());
        for (k, v) in headers {
            req = req.header(k, expand(v));
        }
        if let Some(s) = session.lock().unwrap().clone() {
            req = req.header("mcp-session-id", s);
        }
        if let Some(t) = token
            && !headers.keys().any(|k| k.eq_ignore_ascii_case("authorization"))
        {
            req = req.bearer_auth(t);
        }
        req.send().await.map_err(|e| e.to_string())
    }

    async fn read_answer(&self, resp: reqwest::Response, id: u64, idle: Duration) -> Result<Value, String> {
        let sse = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|c| c.starts_with("text/event-stream"));
        if !sse {
            let v: Value =
                serde_json::from_slice(&resp.bytes().await.map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
            return answer(&v);
        }
        let mut stream = resp.bytes_stream();
        let mut parser = novacode_ai::sse::SseParser::default();
        loop {
            let chunk = tokio::time::timeout(idle, stream.next())
                .await
                .map_err(|_| format!("no answer or progress for {}s", idle.as_secs()))?;
            let Some(chunk) = chunk else {
                return Err("the stream ended without an answer".into());
            };
            for ev in parser.push(&chunk.map_err(|e| e.to_string())?) {
                let Ok(v) = serde_json::from_str::<Value>(&ev.data) else {
                    continue;
                };
                if v["id"].as_u64() == Some(id) {
                    return answer(&v);
                }
            }
        }
    }
}

fn answer(v: &Value) -> Result<Value, String> {
    if let Some(e) = v.get("error") {
        return Err(e["message"].as_str().unwrap_or("error").to_string());
    }
    Ok(v["result"].clone())
}

/// Spawn a stdio server and its reader/writer tasks.
fn spawn_stdio(c: &ServerConfig, cwd: &Path, log: &Path) -> Result<Transport, String> {
    let cmd = c.command.as_deref().ok_or("no command or url")?;
    let mut p = tokio::process::Command::new(expand(cmd));
    p.args(c.args.iter().map(|a| expand(a)))
        .envs(c.env.iter().map(|(k, v)| (k, expand(v))))
        .current_dir(
            c.cwd
                .as_deref()
                .map(|d| cwd.join(expand(d)))
                .unwrap_or_else(|| cwd.to_path_buf()),
        )
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .process_group(0)
        .kill_on_drop(true);
    if let Some(dir) = log.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    p.stderr(
        std::fs::File::create(log)
            .map(std::process::Stdio::from)
            .unwrap_or_else(|_| std::process::Stdio::null()),
    );
    let mut child = p.spawn().map_err(|e| format!("could not start `{cmd}`: {e}"))?;
    let pid = child.id();
    let mut stdin = child.stdin.take().ok_or("no stdin")?;
    let stdout = child.stdout.take().ok_or("no stdout")?;
    let (tx, mut rx) = mpsc::unbounded_channel::<String>();
    let pending: Pending = Arc::default();
    tokio::spawn(async move {
        while let Some(line) = rx.recv().await {
            if stdin.write_all(format!("{line}\n").as_bytes()).await.is_err() {
                break;
            }
            let _ = stdin.flush().await;
        }
    });
    let (p2, tx2) = (pending.clone(), tx.clone());
    tokio::spawn(async move {
        let _child = child;
        let mut lines = BufReader::new(stdout).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let Ok(v) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            match (v.get("id"), v.get("method").and_then(|m| m.as_str())) {
                // A request from the server: answer ping, refuse the rest.
                (Some(id), Some(m)) => {
                    let reply = if m == "ping" {
                        json!({"jsonrpc": "2.0", "id": id, "result": {}})
                    } else {
                        json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": "not supported"}})
                    };
                    let _ = tx2.send(reply.to_string());
                }
                (Some(id), None) => {
                    if let Some((done, _)) = id.as_u64().and_then(|id| p2.lock().unwrap().remove(&id)) {
                        let _ = done.send(answer(&v));
                    }
                }
                (None, Some("notifications/progress")) => {
                    if let Some(t) = v["params"]["progressToken"].as_u64()
                        && let Some((_, n)) = p2.lock().unwrap().get(&t)
                    {
                        n.notify_one();
                    }
                }
                _ => {}
            }
        }
        // The server exited: fail everything still waiting.
        for (_, (done, _)) in p2.lock().unwrap().drain() {
            let _ = done.send(Err("the server has exited".into()));
        }
    });
    Ok(Transport::Stdio { tx, pending, pid })
}

struct Server {
    name: String,
    config: ServerConfig,
    status: watch::Sender<Status>,
    conn: tokio::sync::Mutex<Option<Arc<Conn>>>,
    tools: Mutex<Vec<ToolSpec>>,
}

/// Every configured server of a session.
pub struct Hub {
    servers: Vec<Arc<Server>>,
    cache_dir: PathBuf,
    log_dir: PathBuf,
    cwd: PathBuf,
    home: PathBuf,
}

impl Hub {
    /// Start every server in the background; returns at once.
    pub fn start(servers: Vec<Configured>, home: &Path, cwd: &Path) -> Arc<Hub> {
        let cache_dir = home.join("cache").join("mcp");
        let hub = Arc::new(Hub {
            servers: servers
                .into_iter()
                .map(|c| {
                    let cached: Vec<ToolSpec> =
                        novacode_base::storage::read_json(&cache_dir.join(format!("{}.json", c.name)))
                            .ok()
                            .flatten()
                            .unwrap_or_default();
                    let first = if cached.is_empty() {
                        Status::Connecting
                    } else {
                        Status::Idle
                    };
                    Arc::new(Server {
                        name: c.name,
                        config: c.config,
                        status: watch::channel(first).0,
                        conn: tokio::sync::Mutex::new(None),
                        tools: Mutex::new(cached),
                    })
                })
                .collect(),
            cache_dir,
            log_dir: home.join("logs"),
            cwd: cwd.to_path_buf(),
            home: home.to_path_buf(),
        });
        // Only servers without a cached list start now, so their tools can be found.
        for s in hub.servers.iter().filter(|s| *s.status.borrow() == Status::Connecting) {
            let (hub, s) = (hub.clone(), s.clone());
            tokio::spawn(async move { hub.connect(&s).await });
        }
        hub
    }

    /// Start every server, idle ones too, and wait for each to settle (`novacode mcp list`).
    pub async fn connect_all(&self) {
        for s in &self.servers {
            self.connect(s).await;
        }
    }

    async fn connect(&self, s: &Arc<Server>) -> Option<Arc<Conn>> {
        let mut slot = s.conn.lock().await;
        if let Some(c) = slot.as_ref() {
            return Some(c.clone());
        }
        let _ = s.status.send_replace(Status::Connecting);
        let r = tokio::time::timeout(CONNECT_TIMEOUT, self.handshake(s)).await;
        match r {
            Ok(Ok((conn, tools))) => {
                let path = self.cache_dir.join(format!("{}.json", s.name));
                let _ = novacode_base::storage::write_json(&path, &tools, novacode_base::storage::Durability::Fast);
                *s.tools.lock().unwrap() = tools;
                let _ = s.status.send_replace(Status::Ready);
                *slot = Some(conn.clone());
                Some(conn)
            }
            Ok(Err(e)) => {
                tracing::warn!(server = %s.name, "MCP connect failed: {e}");
                let _ = s.status.send_replace(Status::Failed(e));
                None
            }
            Err(_) => {
                let _ = s.status.send_replace(Status::Failed(format!(
                    "no answer within {}s",
                    CONNECT_TIMEOUT.as_secs()
                )));
                None
            }
        }
    }

    async fn handshake(&self, s: &Server) -> Result<(Arc<Conn>, Vec<ToolSpec>), String> {
        let transport = match &s.config.url {
            Some(url) => Transport::Http {
                url: expand(url),
                headers: s.config.headers.clone(),
                session: Mutex::new(None),
                http: novacode_ai::http::client(),
                auth: Some(crate::mcp_oauth::Auth::new(&self.home, &s.name, &expand(url))),
                name: s.name.clone(),
            },
            None => spawn_stdio(&s.config, &self.cwd, &self.log_dir.join(format!("mcp-{}.log", s.name)))?,
        };
        let conn = Arc::new(Conn {
            transport,
            ids: AtomicU64::new(1),
        });
        let idle = Duration::from_secs(30);
        conn.request(
            "initialize",
            json!({ "protocolVersion": PROTOCOL, "capabilities": {}, "clientInfo": { "name": "novacode", "version": env!("CARGO_PKG_VERSION") } }),
            idle,
        )
        .await?;
        conn.notify("notifications/initialized").await;
        let mut tools = Vec::new();
        let mut cursor: Option<String> = None;
        for _ in 0..50 {
            let params = match &cursor {
                Some(c) => json!({ "cursor": c }),
                None => json!({}),
            };
            let r = conn.request("tools/list", params, idle).await?;
            tools.extend(serde_json::from_value::<Vec<ToolSpec>>(r["tools"].clone()).map_err(|e| e.to_string())?);
            cursor = r["nextCursor"].as_str().map(String::from);
            if cursor.is_none() {
                break;
            }
        }
        Ok((conn, tools))
    }

    /// Tools for the session: each with its exposure, from the cache or a live list.
    pub fn tools(self: &Arc<Self>) -> Vec<Arc<dyn Tool>> {
        let mut out: Vec<Arc<dyn Tool>> = Vec::new();
        for s in &self.servers {
            for t in s.tools.lock().unwrap().iter() {
                let exposure = s
                    .config
                    .tools
                    .get(&t.name)
                    .and_then(|o| o.exposure)
                    .or(s.config.exposure)
                    .unwrap_or(Exposure::CodemodeOnly);
                out.push(Arc::new(McpTool {
                    hub: self.clone(),
                    server: s.name.clone(),
                    spec: t.clone(),
                    exposure,
                    name: format!("{}__{}", s.name, t.name),
                }));
            }
        }
        out
    }

    /// (server, status, tool count, description) for `/mcp` and `novacode mcp list`.
    pub fn status(&self) -> Vec<(String, Status, usize, String)> {
        self.servers
            .iter()
            .map(|s| {
                let d = s.config.description.clone().unwrap_or_default();
                (
                    s.name.clone(),
                    s.status.borrow().clone(),
                    s.tools.lock().unwrap().len(),
                    d,
                )
            })
            .collect()
    }

    /// Wait until every server is ready or failed (CLI listing, tests).
    pub async fn settle(&self) {
        for s in &self.servers {
            let mut rx = s.status.subscribe();
            let _ = rx.wait_for(|st| *st != Status::Connecting).await;
        }
    }

    async fn call(&self, server: &str, tool: &str, args: Value) -> Result<Value, String> {
        let s = self
            .servers
            .iter()
            .find(|s| s.name == server)
            .ok_or("unknown server")?
            .clone();
        let conn = match self.connect(&s).await {
            Some(c) => c,
            None => {
                return Err(format!(
                    "MCP server `{server}` is not connected: {:?}",
                    s.status.borrow().clone()
                ));
            }
        };
        let idle = Duration::from_secs(s.config.timeout.unwrap_or(120));
        let r = conn
            .request("tools/call", json!({ "name": tool, "arguments": args }), idle)
            .await;
        if let Err(e) = &r
            && e.contains("exited")
        {
            // Reconnect on the next call.
            *s.conn.lock().await = None;
            let _ = s.status.send_replace(Status::Failed(e.clone()));
        }
        r
    }
}

pub struct McpTool {
    hub: Arc<Hub>,
    server: String,
    spec: ToolSpec,
    exposure: Exposure,
    name: String,
}

impl Tool for McpTool {
    fn name(&self) -> &str {
        &self.name
    }
    fn description(&self) -> &str {
        &self.spec.description
    }
    fn schema(&self) -> Value {
        match &self.spec.input_schema {
            Value::Object(_) => self.spec.input_schema.clone(),
            _ => json!({ "type": "object", "properties": {} }),
        }
    }
    fn exposure(&self) -> Exposure {
        self.exposure
    }
    fn read_only(&self) -> bool {
        self.spec.annotations["readOnlyHint"] == true
    }
    fn execute<'a>(&'a self, args: Value, ctx: &'a ToolCtx) -> BoxFuture<'a, ToolOutput> {
        Box::pin(async move {
            let call = self.hub.call(&self.server, &self.spec.name, args);
            let r = tokio::select! {
                r = call => r,
                _ = ctx.cancel.cancelled() => return ToolOutput::error("cancelled"),
            };
            match r {
                Err(e) => ToolOutput::error(format!("{}: {e}", self.name)),
                Ok(v) => to_output(&v),
            }
        })
    }
}

/// MCP content → tool output; structured content goes to details for codemode.
fn to_output(v: &Value) -> ToolOutput {
    let mut content = Vec::new();
    for c in v["content"].as_array().into_iter().flatten() {
        match c["type"].as_str() {
            Some("text") => content.push(Block::text(c["text"].as_str().unwrap_or_default().to_string())),
            Some("image") => content.push(Block::Image {
                data: c["data"].as_str().unwrap_or_default().to_string(),
                mime_type: c["mimeType"].as_str().unwrap_or("image/png").to_string(),
            }),
            Some("resource") => content.push(Block::text(
                c["resource"]["text"]
                    .as_str()
                    .map(String::from)
                    .unwrap_or_else(|| c["resource"].to_string()),
            )),
            _ => content.push(Block::text(c.to_string())),
        }
    }
    let structured = v.get("structuredContent").filter(|s| !s.is_null()).cloned();
    if content.is_empty() {
        content.push(Block::text(
            structured
                .as_ref()
                .map(|s| s.to_string())
                .unwrap_or_else(|| "(no output)".into()),
        ));
    }
    ToolOutput {
        content,
        is_error: v["isError"].as_bool().unwrap_or(false),
        details: structured.map(|s| json!({ "structured": s })),
    }
}

/// Add or remove a server in the global `mcp.json`, keeping other keys.
pub fn edit_global(home: &Path, name: &str, server: Option<Value>) -> std::io::Result<bool> {
    let path = home.join("mcp.json");
    let mut root: Map<String, Value> = std::fs::read_to_string(&path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default();
    let servers = root.entry("mcpServers").or_insert_with(|| json!({}));
    let Some(map) = servers.as_object_mut() else {
        return Ok(false);
    };
    let changed = match server {
        Some(v) => {
            map.insert(name.to_string(), v);
            true
        }
        None => map.remove(name).is_some(),
    };
    if changed {
        novacode_base::storage::write_json(&path, &root, novacode_base::storage::Durability::Durable)?;
    }
    Ok(changed)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tiny stdio MCP server written in Python.
    const SERVER: &str = r#"
import json, sys
for line in sys.stdin:
    m = json.loads(line)
    if "id" not in m: continue
    if m["method"] == "initialize": r = {"protocolVersion": "2025-06-18", "capabilities": {"tools": {}}, "serverInfo": {"name": "t", "version": "1"}}
    elif m["method"] == "tools/list": r = {"tools": [{"name": "add", "description": "Add two numbers", "inputSchema": {"type": "object", "properties": {"a": {"type": "number"}, "b": {"type": "number"}}}}]}
    elif m["method"] == "tools/call":
        a = m["params"]["arguments"]
        if m["params"]["name"] == "add": r = {"content": [{"type": "text", "text": str(a["a"] + a["b"])}], "structuredContent": {"sum": a["a"] + a["b"]}}
    print(json.dumps({"jsonrpc": "2.0", "id": m["id"], "result": r}), flush=True)
"#;

    #[tokio::test]
    async fn stdio_server_lists_calls_and_caches() {
        let home = tempfile::tempdir().unwrap();
        let script = home.path().join("srv.py");
        std::fs::write(&script, SERVER).unwrap();
        std::fs::write(
            home.path().join("mcp.json"),
            json!({"mcpServers": {"calc": {"command": "python3", "args": [script]}, "bad__name": {"command": "x"}}})
                .to_string(),
        )
        .unwrap();
        let (servers, warn) = load(home.path(), home.path(), false);
        assert_eq!(servers.len(), 1);
        assert_eq!(warn.len(), 1);
        let hub = Hub::start(servers.clone(), home.path(), home.path());
        // Nobody watches the status while it connects: settle must still see it finish.
        tokio::time::sleep(Duration::from_secs(2)).await;
        tokio::time::timeout(Duration::from_secs(5), hub.settle())
            .await
            .expect("settle saw the finished connect");
        let tools = hub.tools();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name(), "calc__add");
        assert_eq!(tools[0].exposure(), Exposure::CodemodeOnly);
        let ctx = ToolCtx {
            cwd: home.path().into(),
            env: Arc::new(novacode_tools::LocalEnv),
            cancel: Default::default(),
            updates: None,
            files: Default::default(),
            session_id: "t".into(),
            processes: None,
            native_tools: false,
            nested: None,
            script: false,
        };
        let out = tools[0].execute(json!({"a": 2, "b": 3}), &ctx).await;
        assert!(!out.is_error);
        assert_eq!(out.details.unwrap()["structured"]["sum"], 5);
        // A new session sees the tools from the cache before connecting.
        let cached = Hub::start(
            vec![Configured {
                config: ServerConfig {
                    command: Some("false".into()),
                    ..Default::default()
                },
                ..servers[0].clone()
            }],
            home.path(),
            home.path(),
        );
        assert_eq!(cached.tools().len(), 1);
        assert_eq!(
            cached.status()[0].1,
            Status::Idle,
            "a cached server waits for its first call"
        );
        let out = cached.tools()[0].execute(json!({"a": 2, "b": 3}), &ctx).await;
        assert!(out.is_error);
        assert!(matches!(cached.status()[0].1, Status::Failed(_)));
    }

    /// L7.4 revision: an uncached server starts with the session; a cached one only at
    /// its first call, and concurrent first calls share one start.
    #[tokio::test]
    async fn cached_servers_start_on_first_use() {
        let home = tempfile::tempdir().unwrap();
        let script = home.path().join("srv.py");
        let marker = home.path().join("starts");
        std::fs::write(
            &script,
            format!("import os\nopen(os.environ['MARK'], 'a').write('x')\n{SERVER}"),
        )
        .unwrap();
        let servers = vec![Configured {
            name: "calc".into(),
            config: ServerConfig {
                command: Some("python3".into()),
                args: vec![script.display().to_string()],
                env: [("MARK".to_string(), marker.display().to_string())].into(),
                ..Default::default()
            },
            source: home.path().join("mcp.json"),
        }];
        let starts = || std::fs::read_to_string(&marker).unwrap_or_default().len();
        let first = Hub::start(servers.clone(), home.path(), home.path());
        tokio::time::timeout(Duration::from_secs(10), first.settle())
            .await
            .unwrap();
        assert_eq!(
            (starts(), first.status()[0].1.clone()),
            (1, Status::Ready),
            "no cache: starts at once"
        );
        drop(first);
        let hub = Hub::start(servers, home.path(), home.path());
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert_eq!(
            (starts(), hub.status()[0].1.clone()),
            (1, Status::Idle),
            "cached: not started"
        );
        let ctx = ToolCtx {
            cwd: home.path().into(),
            env: Arc::new(novacode_tools::LocalEnv),
            cancel: Default::default(),
            updates: None,
            files: Default::default(),
            session_id: "t".into(),
            processes: None,
            native_tools: false,
            nested: None,
            script: false,
        };
        let tools = hub.tools();
        let (a, b) = tokio::join!(
            tools[0].execute(json!({"a": 1, "b": 2}), &ctx),
            tools[0].execute(json!({"a": 3, "b": 4}), &ctx)
        );
        assert!(!a.is_error && !b.is_error);
        assert_eq!(
            (starts(), hub.status()[0].1.clone()),
            (2, Status::Ready),
            "one start for both calls"
        );
    }

    #[test]
    fn expands_env() {
        assert_eq!(expand("a${NOVACODE_NOPE:-x}b"), "axb");
    }
}
