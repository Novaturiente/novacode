//! Diagnostics after edits: language servers tell the model what its edit broke.
//!
//! Designed for novacode rather than ported from pi-lens:
//! - One job: after `edit`/`write`/`apply_patch` (or a `bash` command that changed
//!   files), the errors in the touched files go into that tool's result (stored
//!   once, so the cache holds).
//! - Real language servers, started lazily and kept warm. A `read` already
//!   starts the server and opens the file, so its errors are known before the
//!   edit and only what changed is worth saying.
//! - Bounded: waits at most `waitMs` per call, lists at most `maxItems`, and
//!   stays silent when nothing changed.
//! - Configurable (`extensions.lsp` in settings): `enabled`, `waitMs`, `severity`,
//!   `maxItems`, and `servers.<name>` to add, replace or turn off (`false`) a server.

use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{Notify, oneshot, watch};
use tokio::time::Instant;

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Server {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    /// File extensions it handles, without the dot.
    pub extensions: Vec<String>,
    /// Files that mark a project root (nearest one up from the file wins).
    #[serde(default)]
    pub root_markers: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Config {
    pub enabled: bool,
    pub wait_ms: u64,
    /// `error`, or `warning` to list warnings too.
    pub severity: String,
    pub max_items: usize,
    /// Added to, or replacing, the built-ins by name; `false` turns one off.
    pub servers: BTreeMap<String, Value>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            enabled: true,
            wait_ms: 3000,
            severity: "error".into(),
            max_items: 10,
            servers: BTreeMap::new(),
        }
    }
}

fn builtins() -> Vec<(&'static str, Server)> {
    let s = |cmd: &str, args: &[&str], ext: &[&str], markers: &[&str]| Server {
        command: cmd.into(),
        args: args.iter().map(|a| a.to_string()).collect(),
        extensions: ext.iter().map(|a| a.to_string()).collect(),
        root_markers: markers.iter().map(|a| a.to_string()).collect(),
    };
    vec![
        ("rust", s("rust-analyzer", &[], &["rs"], &["Cargo.toml"])),
        (
            "typescript",
            s(
                "typescript-language-server",
                &["--stdio"],
                &["ts", "tsx", "js", "jsx", "mjs", "cjs", "mts", "cts"],
                &["tsconfig.json", "jsconfig.json", "package.json"],
            ),
        ),
        (
            "python",
            s(
                "pyright-langserver",
                &["--stdio"],
                &["py", "pyi"],
                &["pyproject.toml", "pyrightconfig.json", "setup.py", "requirements.txt"],
            ),
        ),
        ("go", s("gopls", &[], &["go"], &["go.mod"])),
        (
            "c",
            s(
                "clangd",
                &[],
                &["c", "h", "cc", "cpp", "cxx", "hpp", "hh"],
                &["compile_commands.json", "CMakeLists.txt", "Makefile"],
            ),
        ),
    ]
}

/// The servers to use: built-ins on PATH, then settings by name.
pub fn servers(cfg: &Config) -> (Vec<(String, Server)>, Vec<String>) {
    let mut out: BTreeMap<String, Server> = builtins().into_iter().map(|(n, s)| (n.to_string(), s)).collect();
    let mut warn = Vec::new();
    for (name, v) in &cfg.servers {
        match v {
            Value::Bool(false) | Value::Null => {
                out.remove(name);
            }
            v => match serde_json::from_value::<Server>(v.clone()) {
                Ok(s) => {
                    out.insert(name.clone(), s);
                }
                Err(e) => warn.push(format!("lsp.servers.{name}: {e}")),
            },
        }
    }
    let list = out.into_iter().filter(|(_, s)| on_path(&s.command)).collect();
    (list, warn)
}

fn on_path(cmd: &str) -> bool {
    if cmd.contains('/') {
        return Path::new(cmd).is_file();
    }
    std::env::var_os("PATH").is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(cmd).is_file()))
}

fn language_id(ext: &str) -> &str {
    match ext {
        "rs" => "rust",
        "ts" | "mts" | "cts" => "typescript",
        "tsx" => "typescriptreact",
        "js" | "mjs" | "cjs" => "javascript",
        "jsx" => "javascriptreact",
        "py" | "pyi" => "python",
        "c" | "h" => "c",
        "cc" | "cpp" | "cxx" | "hpp" | "hh" => "cpp",
        e => e,
    }
}

pub fn uri(p: &Path) -> String {
    let mut s = String::from("file://");
    for b in p.to_string_lossy().bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'/' | b'-' | b'_' | b'.' | b'~' => s.push(b as char),
            b => s.push_str(&format!("%{b:02X}")),
        }
    }
    s
}

fn path_of(uri: &str) -> PathBuf {
    let raw = uri.strip_prefix("file://").unwrap_or(uri).as_bytes();
    let mut out = Vec::with_capacity(raw.len());
    let mut i = 0;
    while i < raw.len() {
        if raw[i] == b'%'
            && let Some(b) = std::str::from_utf8(raw.get(i + 1..i + 3).unwrap_or_default())
                .ok()
                .and_then(|h| u8::from_str_radix(h, 16).ok())
        {
            out.push(b);
            i += 3;
            continue;
        }
        out.push(raw[i]);
        i += 1;
    }
    PathBuf::from(String::from_utf8_lossy(&out).into_owned())
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Diag {
    pub line: u32,
    pub col: u32,
    pub severity: u8,
    pub message: String,
    /// Who found it: `rust-analyzer` for its own checks, `rustc` for cargo check.
    pub source: String,
}

#[derive(Default)]
struct Published {
    /// Bumped by every publish for the file.
    count: u64,
    diags: Vec<Diag>,
}

/// One running language server for one project root.
struct Client {
    stdin: tokio::sync::Mutex<tokio::process::ChildStdin>,
    next: AtomicI64,
    pending: Mutex<HashMap<i64, oneshot::Sender<Value>>>,
    published: Mutex<HashMap<String, Published>>,
    changed: Notify,
    /// uri → version of open documents.
    open: Mutex<HashMap<String, i32>>,
    /// Work-done progress tokens still running (`$/progress` begin without end).
    progress: Mutex<HashSet<String>>,
    /// rust-analyzer's `experimental/serverStatus`: false while it loads or indexes.
    quiescent: Mutex<Option<bool>>,
    /// rust-analyzer's health message when not "ok" (e.g. no std sources).
    unhealthy: Mutex<Option<String>>,
    /// `unhealthy` was told to the model.
    warned: AtomicBool,
    _child: Mutex<tokio::process::Child>,
}

impl Client {
    async fn start(s: &Server, root: &Path, log: &Path) -> Result<Arc<Client>, String> {
        let mut cmd = tokio::process::Command::new(&s.command);
        cmd.args(&s.args)
            .current_dir(root)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(match std::fs::File::create(log) {
                Ok(f) => std::process::Stdio::from(f),
                Err(_) => std::process::Stdio::null(),
            })
            .process_group(0)
            .kill_on_drop(true);
        let mut child = cmd.spawn().map_err(|e| format!("{}: {e}", s.command))?;
        let stdin = child.stdin.take().ok_or("no stdin")?;
        let stdout = child.stdout.take().ok_or("no stdout")?;
        let c = Arc::new(Client {
            stdin: tokio::sync::Mutex::new(stdin),
            next: AtomicI64::new(1),
            pending: Mutex::default(),
            published: Mutex::default(),
            changed: Notify::new(),
            open: Mutex::default(),
            progress: Mutex::default(),
            quiescent: Mutex::default(),
            unhealthy: Mutex::default(),
            warned: AtomicBool::new(false),
            _child: Mutex::new(child),
        });
        tokio::spawn(read_loop(Arc::downgrade(&c), BufReader::new(stdout)));
        let root_uri = uri(root);
        let init = c.request(
            "initialize",
            json!({
                "processId": std::process::id(),
                "rootUri": root_uri,
                "workspaceFolders": [{ "uri": root_uri, "name": root.file_name().map(|n| n.to_string_lossy()).unwrap_or_default() }],
                "capabilities": {
                    "textDocument": {
                        "synchronization": { "didSave": true },
                        "publishDiagnostics": { "versionSupport": true }
                    },
                    "workspace": { "configuration": true, "workspaceFolders": true },
                    "window": { "workDoneProgress": true },
                    "experimental": { "serverStatusNotification": true }
                }
            }),
        );
        tokio::time::timeout(Duration::from_secs(30), init)
            .await
            .map_err(|_| format!("{} didn't answer initialize in 30 s", s.command))??;
        c.notify("initialized", json!({})).await;
        Ok(c)
    }

    async fn send(&self, msg: Value) {
        let body = msg.to_string();
        let mut w = self.stdin.lock().await;
        let _ = w
            .write_all(format!("Content-Length: {}\r\n\r\n{body}", body.len()).as_bytes())
            .await;
        let _ = w.flush().await;
    }

    async fn notify(&self, method: &str, params: Value) {
        self.send(json!({ "jsonrpc": "2.0", "method": method, "params": params }))
            .await;
    }

    async fn request(&self, method: &str, params: Value) -> Result<Value, String> {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, tx);
        self.send(json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }))
            .await;
        rx.await.map_err(|_| format!("the server closed during {method}"))
    }

    fn snapshot(&self, uri: &str) -> (u64, Vec<Diag>) {
        let p = self.published.lock().unwrap();
        p.get(uri).map_or((0, Vec::new()), |x| (x.count, x.diags.clone()))
    }

    /// Open or update `path` with its current text and save it (some servers,
    /// e.g. rust-analyzer's cargo check, report only on save).
    async fn sync(&self, path: &Path, ext: &str) -> Option<String> {
        let text = tokio::fs::read_to_string(path).await.ok()?;
        let u = uri(path);
        let version = {
            let mut open = self.open.lock().unwrap();
            let v = open.entry(u.clone()).or_insert(0);
            *v += 1;
            *v
        };
        if version == 1 {
            self.notify(
                "textDocument/didOpen",
                json!({ "textDocument": { "uri": u, "languageId": language_id(ext), "version": 1, "text": text } }),
            )
            .await;
        } else {
            self.notify(
                "textDocument/didChange",
                json!({ "textDocument": { "uri": u, "version": version }, "contentChanges": [{ "text": text }] }),
            )
            .await;
        }
        self.notify("textDocument/didSave", json!({ "textDocument": { "uri": u } }))
            .await;
        Some(u)
    }

    /// While rust-analyzer is unhealthy (no std sources, broken workspace) its own
    /// type checks are wrong; cargo check's (`rustc`) still hold.
    fn trusted(&self, diags: Vec<Diag>) -> Vec<Diag> {
        if self.unhealthy.lock().unwrap().is_none() {
            return diags;
        }
        diags.into_iter().filter(|d| d.source != "rust-analyzer").collect()
    }

    /// No background work running. Diagnostics published while a server is still
    /// loading the project are wrong (rust-analyzer without its sysroot says
    /// "cannot index into `&[u8]`"), so only these count.
    fn ready(&self) -> bool {
        self.progress.lock().unwrap().is_empty() && *self.quiescent.lock().unwrap() != Some(false)
    }

    /// Wait until the server is ready and has published for `uri` after `since`,
    /// then a short quiet spell for follow-ups (servers often publish twice), all
    /// by `end`. False: still loading when the time ran out.
    async fn settle(&self, uri: &str, since: u64, end: Instant) -> bool {
        let mut seen = since;
        let mut until = end;
        loop {
            let n = self.changed.notified();
            let now = self.snapshot(uri).0;
            if !self.ready() {
                until = end;
            } else if now > seen {
                seen = now;
                // Something arrived: give stragglers 300 ms, not the whole wait. Other
                // files' publishes wake us too; they must not reset this to `end`.
                until = (Instant::now() + Duration::from_millis(300)).min(end);
            }
            if tokio::time::timeout_at(until, n).await.is_err() {
                return self.ready();
            }
        }
    }
}

/// Server messages span lines ("mismatched types\nexpected u32, found &str"): keep them all, short.
fn one_line(m: &str) -> String {
    let s = m.split_whitespace().collect::<Vec<_>>().join(" ");
    match s.char_indices().nth(240) {
        Some((i, _)) => format!("{}…", &s[..i]),
        None => s,
    }
}

async fn read_loop(c: std::sync::Weak<Client>, r: BufReader<tokio::process::ChildStdout>) {
    read_messages(&c, r).await;
    // The server is gone: fail its waiting requests now, not at their timeout.
    if let Some(c) = c.upgrade() {
        c.pending.lock().unwrap().clear();
    }
}

async fn read_messages(c: &std::sync::Weak<Client>, mut r: BufReader<tokio::process::ChildStdout>) {
    loop {
        let mut len = 0usize;
        loop {
            let mut line = String::new();
            match r.read_line(&mut line).await {
                Ok(0) | Err(_) => return,
                Ok(_) => {}
            }
            let line = line.trim_end();
            if line.is_empty() {
                break;
            }
            if let Some(v) = line.strip_prefix("Content-Length:") {
                len = v.trim().parse().unwrap_or(0);
            }
        }
        let mut buf = vec![0; len];
        if r.read_exact(&mut buf).await.is_err() {
            return;
        }
        let Some(c) = c.upgrade() else { return };
        let Ok(msg) = serde_json::from_slice::<Value>(&buf) else {
            continue;
        };
        match (msg.get("id"), msg.get("method").and_then(Value::as_str)) {
            // A response to us.
            (Some(id), None) => {
                if let Some(tx) = id.as_i64().and_then(|i| c.pending.lock().unwrap().remove(&i)) {
                    let _ = tx.send(msg.get("result").cloned().unwrap_or(Value::Null));
                }
            }
            // A request from the server: it waits for an answer.
            (Some(id), Some(method)) => {
                let result = match method {
                    "workspace/configuration" => {
                        let n = msg["params"]["items"].as_array().map_or(1, Vec::len);
                        Value::Array(vec![Value::Null; n])
                    }
                    "workspace/workspaceFolders" => Value::Array(Vec::new()),
                    _ => Value::Null,
                };
                c.send(json!({ "jsonrpc": "2.0", "id": id, "result": result })).await;
            }
            (None, Some("$/progress")) => {
                let p = &msg["params"];
                let token = p["token"].to_string();
                match p["value"]["kind"].as_str() {
                    Some("begin") => c.progress.lock().unwrap().insert(token),
                    Some("end") => c.progress.lock().unwrap().remove(&token),
                    _ => continue,
                };
                c.changed.notify_waiters();
            }
            (None, Some("experimental/serverStatus")) => {
                let p = &msg["params"];
                *c.quiescent.lock().unwrap() = p["quiescent"].as_bool();
                *c.unhealthy.lock().unwrap() =
                    (p["health"] != "ok").then(|| p["message"].as_str().unwrap_or("unhealthy").to_string());
                c.changed.notify_waiters();
            }
            (None, Some("textDocument/publishDiagnostics")) => {
                let p = &msg["params"];
                let u = p["uri"].as_str().unwrap_or_default().to_string();
                let mut diags: Vec<Diag> = p["diagnostics"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|d| Diag {
                        line: d["range"]["start"]["line"].as_u64().unwrap_or(0) as u32 + 1,
                        col: d["range"]["start"]["character"].as_u64().unwrap_or(0) as u32 + 1,
                        severity: d["severity"].as_u64().unwrap_or(1) as u8,
                        message: one_line(d["message"].as_str().unwrap_or_default()),
                        source: d["source"].as_str().unwrap_or_default().to_string(),
                    })
                    .collect();
                diags.sort();
                diags.dedup();
                let mut pubs = c.published.lock().unwrap();
                let e = pubs.entry(u).or_default();
                e.count += 1;
                e.diags = diags;
                drop(pubs);
                c.changed.notify_waiters();
            }
            _ => {}
        }
    }
}

/// A started client, or why it couldn't start (never retried).
type Started = Result<Arc<Client>, String>;

/// One (server, root): started in the background, so no check waits past its deadline.
struct Slot {
    /// `None` while the server starts.
    started: watch::Receiver<Option<Started>>,
    /// The start failure was told to the model (said once).
    told: AtomicBool,
}

/// The after-tool handler: one client per (server, root), started on first use.
pub struct Lsp {
    cfg: Config,
    servers: Vec<(String, Server)>,
    clients: Mutex<HashMap<(String, PathBuf), Arc<Slot>>>,
    /// What the model was last told per file: (errors, warnings) it saw.
    told: Mutex<HashMap<String, Vec<Diag>>>,
    cwd: PathBuf,
    logs: PathBuf,
}

impl Lsp {
    pub fn new(cfg: Config, cwd: &Path, logs: &Path) -> (Arc<Lsp>, Vec<String>) {
        let (servers, warn) = servers(&cfg);
        let lsp = Lsp {
            cfg,
            servers,
            clients: Default::default(),
            told: Default::default(),
            cwd: cwd.to_path_buf(),
            logs: logs.to_path_buf(),
        };
        (Arc::new(lsp), warn)
    }

    pub fn has_servers(&self) -> bool {
        self.cfg.enabled && !self.servers.is_empty()
    }

    fn server_for(&self, path: &Path) -> Option<(&str, &Server, String)> {
        let ext = path.extension()?.to_str()?.to_ascii_lowercase();
        self.servers
            .iter()
            .find(|(_, s)| s.extensions.contains(&ext))
            .map(|(n, s)| (n.as_str(), s, ext))
    }

    fn root(&self, s: &Server, path: &Path) -> PathBuf {
        path.ancestors()
            .skip(1)
            .find(|d| s.root_markers.iter().any(|m| d.join(m).exists()))
            .map_or_else(|| self.cwd.clone(), Path::to_path_buf)
    }

    fn slot(&self, name: &str, s: &Server, root: PathBuf) -> Arc<Slot> {
        let mut g = self.clients.lock().unwrap();
        g.entry((name.to_string(), root.clone()))
            .or_insert_with(|| {
                let (tx, rx) = watch::channel(None);
                let (s, name, logs) = (s.clone(), name.to_string(), self.logs.clone());
                tokio::spawn(async move {
                    let _ = std::fs::create_dir_all(&logs);
                    let c = Client::start(&s, &root, &logs.join(format!("lsp-{name}.log"))).await;
                    if let Err(e) = &c {
                        eprintln!("language server {name}: {e}");
                    }
                    let _ = tx.send(Some(c));
                });
                Arc::new(Slot {
                    started: rx,
                    told: AtomicBool::new(false),
                })
            })
            .clone()
    }

    /// The client once started, waiting at most until `end`; `None`: still starting.
    async fn client(slot: &Slot, end: Instant) -> Option<Started> {
        let mut rx = slot.started.clone();
        match tokio::time::timeout_at(end, rx.wait_for(Option::is_some)).await {
            Err(_) => None,
            Ok(Ok(v)) => v.clone(),
            Ok(Err(_)) => Some(Err("its start task ended".into())),
        }
    }

    /// Diagnostics for the files a tool touched, as text for the model.
    pub async fn check(&self, paths: &[PathBuf], baseline_only: bool) -> Option<String> {
        let max_sev = if self.cfg.severity == "warning" { 2 } else { 1 };
        // One deadline for the whole call: novacode disables an extension that
        // answers after `timeoutMs`, however many files were touched.
        let end = Instant::now() + Duration::from_millis(self.cfg.wait_ms);
        let mut report = Vec::new();
        let mut failed = Vec::new();
        // Send every file first, so servers work on them while we wait.
        let mut synced = Vec::new();
        for path in paths {
            let Some((name, s, ext)) = self.server_for(path) else {
                continue;
            };
            let slot = self.slot(name, s, self.root(s, path));
            let c = match Self::client(&slot, end).await {
                Some(Ok(c)) => c,
                // Kept for the first edit when a `read` sees it: reads report nothing.
                Some(Err(e)) => {
                    if !baseline_only && !slot.told.swap(true, Ordering::Relaxed) {
                        failed.push(e);
                    }
                    continue;
                }
                None => {
                    if !baseline_only {
                        let rel = path.strip_prefix(&self.cwd).unwrap_or(path).display().to_string();
                        report.push(format!("{rel}: not checked, {name} is still starting"));
                    }
                    continue;
                }
            };
            let u = uri(path);
            let before = c.snapshot(&u).0;
            if c.sync(path, &ext).await.is_none() {
                continue;
            }
            synced.push((path, name, c, u, before));
        }
        for (path, name, c, u, before) in synced {
            if !c.settle(&u, before, end).await {
                // Saying nothing would read as "no errors"; old diagnostics stay as the baseline.
                if !baseline_only {
                    let rel = path.strip_prefix(&self.cwd).unwrap_or(path).display().to_string();
                    report.push(format!("{rel}: not checked, {name} is still loading the project"));
                }
                continue;
            }
            if !baseline_only
                && let Some(m) = c.unhealthy.lock().unwrap().as_deref()
                && !c.warned.swap(true, Ordering::Relaxed)
            {
                report.push(format!("({name}: {}; only cargo check errors are shown)", one_line(m)));
            }
            if baseline_only {
                let diags = c.trusted(c.snapshot(&u).1);
                self.told.lock().unwrap().insert(u, diags);
                continue;
            }
            // The touched file, plus other files whose error count grew (a broken caller).
            let pubs: Vec<(String, Vec<Diag>)> = {
                let p = c.published.lock().unwrap();
                p.iter().map(|(k, v)| (k.clone(), v.diags.clone())).collect()
            };
            let pubs: Vec<(String, Vec<Diag>)> = pubs.into_iter().map(|(k, d)| (k, c.trusted(d))).collect();
            for (file, diags) in pubs {
                let shown: Vec<Diag> = diags.into_iter().filter(|d| d.severity <= max_sev).collect();
                let mut told = self.told.lock().unwrap();
                let prev = told.insert(file.clone(), shown.clone()).unwrap_or_default();
                if file != u && shown.len() <= prev.len() {
                    continue;
                }
                if shown == prev {
                    continue;
                }
                let rel = path_of(&file);
                let rel = rel.strip_prefix(&self.cwd).unwrap_or(&rel).display().to_string();
                if shown.is_empty() {
                    report.push(format!("{rel}: clean now"));
                    continue;
                }
                let new = shown
                    .iter()
                    .filter(|d| !prev.iter().any(|p| p.message == d.message))
                    .count();
                report.push(format!(
                    "{rel}: {} problem{}{}",
                    shown.len(),
                    if shown.len() == 1 { "" } else { "s" },
                    if new < shown.len() {
                        format!(" ({new} new)")
                    } else {
                        String::new()
                    }
                ));
                for d in shown.iter().take(self.cfg.max_items) {
                    let kind = if d.severity == 1 { "error" } else { "warning" };
                    report.push(format!("  {}:{} {kind}: {}", d.line, d.col, d.message));
                }
                if shown.len() > self.cfg.max_items {
                    report.push(format!("  … {} more", shown.len() - self.cfg.max_items));
                }
            }
        }
        // A server that can't start is said once (the error is cached with the client).
        for e in failed {
            report.push(format!("(language server unavailable: {e})"));
        }
        (!report.is_empty()).then(|| report.join("\n"))
    }
}

impl Lsp {
    /// One `after_tool` event: `touched` from novacode; `read` only warms the
    /// server and learns the file's current errors, so the edit after it reports
    /// only what changed.
    pub async fn after_tool(&self, tool: &str, is_error: bool, touched: Vec<PathBuf>) -> Option<String> {
        if !self.has_servers() || is_error {
            return None;
        }
        let paths: Vec<PathBuf> = touched.into_iter().filter(|p| self.server_for(p).is_some()).collect();
        if paths.is_empty() {
            return None;
        }
        if tool == "read" {
            // shortcut: holds the read for up to waitMs on first open; answer early and finish in a task if it shows in timings.
            self.check(&paths, true).await;
            return None;
        }
        self.check(&paths, false).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uris_round_trip() {
        let p = Path::new("/tmp/a b/ü.rs");
        assert_eq!(uri(p), "file:///tmp/a%20b/%C3%BC.rs");
        assert_eq!(path_of(&uri(p)), p);
    }

    #[test]
    fn settings_add_replace_and_disable() {
        let cfg: Config = serde_json::from_value(json!({
            "servers": {
                "rust": false,
                "sh": { "command": "/bin/sh", "extensions": ["sh"] },
                "bad": { "command": 1 }
            }
        }))
        .unwrap();
        let (list, warn) = servers(&cfg);
        assert!(list.iter().all(|(n, _)| n != "rust"));
        assert!(list.iter().any(|(n, _)| n == "sh"));
        assert_eq!(warn.len(), 1, "{warn:?}");
    }

    /// rust-analyzer publishes bogus errors (no sysroot yet: "cannot index into
    /// `&[u8]`") while it loads; a cold check must wait or say it didn't check.
    #[tokio::test]
    async fn rust_analyzer_cold_start_reports_no_false_errors() {
        if !on_path("rust-analyzer") {
            return;
        }
        let d = tempfile::tempdir().unwrap();
        std::fs::write(
            d.path().join("Cargo.toml"),
            "[package]\nname = \"t\"\nedition = \"2021\"\n",
        )
        .unwrap();
        std::fs::create_dir(d.path().join("src")).unwrap();
        let f = d.path().join("src/lib.rs");
        std::fs::write(&f, "pub fn first(b: &[u8]) -> u8 { b[0] }\n").unwrap();
        let (lsp, _) = Lsp::new(Config::default(), d.path(), &d.path().join("logs"));
        let r = lsp.check(std::slice::from_ref(&f), false).await;
        assert!(!r.unwrap_or_default().contains("cannot index"));
    }

    /// A scripted server (`tests/fake_lsp.py`, needs python3) in `mode`, for `.fk` files.
    fn fake(mode: &str, wait_ms: u64, files: usize) -> Option<(tempfile::TempDir, Arc<Lsp>, Vec<PathBuf>)> {
        if !on_path("python3") {
            return None;
        }
        let d = tempfile::tempdir().unwrap();
        let script = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fake_lsp.py");
        let cfg: Config = serde_json::from_value(json!({
            "waitMs": wait_ms,
            "servers": { "fake": { "command": "python3", "args": [script, mode], "extensions": ["fk"] } }
        }))
        .unwrap();
        let (lsp, _) = Lsp::new(cfg, d.path(), &d.path().join("logs"));
        let paths = (0..files)
            .map(|i| {
                let p = d.path().join(format!("f{i}.fk"));
                std::fs::write(&p, "x\n").unwrap();
                p
            })
            .collect();
        Some((d, lsp, paths))
    }

    /// novacode disables the extension past `timeoutMs`: many files still share one wait.
    #[tokio::test]
    async fn many_files_share_one_wait() {
        let Some((_d, lsp, paths)) = fake("loading", 500, 4) else {
            return;
        };
        let t = std::time::Instant::now();
        let r = lsp.check(&paths, false).await.unwrap();
        assert!(t.elapsed() < Duration::from_millis(1500), "{:?}", t.elapsed());
        assert_eq!(r.matches("still loading").count(), 4, "{r}");
    }

    /// Publishes for other files must not stretch a file's 300 ms quiet spell to the whole wait.
    #[tokio::test]
    async fn ready_server_answers_without_the_whole_wait() {
        let Some((_d, lsp, paths)) = fake("errors", 5000, 3) else {
            return;
        };
        let t = std::time::Instant::now();
        let r = lsp.check(&paths, false).await.unwrap();
        assert!(t.elapsed() < Duration::from_millis(3000), "{:?}", t.elapsed());
        assert_eq!(r.matches("1 problem").count(), 3, "{r}");
    }

    /// A server that dies fails at once, and a `read` doesn't use up the one notice.
    #[tokio::test]
    async fn start_failure_is_told_on_the_first_edit() {
        let Some((_d, lsp, paths)) = fake("crash", 3000, 1) else {
            return;
        };
        let t = std::time::Instant::now();
        assert_eq!(lsp.check(&paths, true).await, None, "a read reports nothing");
        assert!(t.elapsed() < Duration::from_secs(2), "{:?}", t.elapsed());
        let r = lsp.check(&paths, false).await.unwrap();
        assert!(r.contains("language server unavailable"), "{r}");
        assert_eq!(lsp.check(&paths, false).await, None, "said once");
    }

    #[tokio::test]
    async fn unhealthy_notice_survives_a_read() {
        let Some((_d, lsp, paths)) = fake("unhealthy", 3000, 1) else {
            return;
        };
        assert_eq!(lsp.check(&paths, true).await, None);
        let r = lsp.check(&paths, false).await.unwrap();
        assert!(r.contains("no std sources"), "{r}");
        assert_eq!(lsp.check(&paths, false).await, None, "said once");
    }

    /// A slow start doesn't hold the call: the file is "not checked" until it's up.
    #[tokio::test]
    async fn slow_start_is_not_waited_for() {
        let Some((_d, lsp, paths)) = fake("slow", 300, 1) else {
            return;
        };
        let t = std::time::Instant::now();
        let r = lsp.check(&paths, false).await.unwrap();
        assert!(t.elapsed() < Duration::from_secs(1), "{:?}", t.elapsed());
        assert!(r.contains("still starting"), "{r}");
    }

    /// A real server, when this machine has one: an edit that breaks the file is reported.
    #[tokio::test]
    async fn pyright_reports_an_error_when_installed() {
        if !on_path("pyright-langserver") {
            return;
        }
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("m.py");
        std::fs::write(&f, "x: int = 1\n").unwrap();
        let (lsp, _) = Lsp::new(
            Config {
                wait_ms: 15000,
                ..Default::default()
            },
            d.path(),
            &d.path().join("logs"),
        );
        assert_eq!(
            lsp.check(std::slice::from_ref(&f), true).await,
            None,
            "baseline is silent"
        );
        std::fs::write(&f, "x: int = 'one'\n").unwrap();
        let r = lsp.check(std::slice::from_ref(&f), false).await.unwrap();
        assert!(r.contains("m.py: 1 problem") && r.contains("1:10 error"), "{r}");
        std::fs::write(&f, "x: int = 2\n").unwrap();
        assert_eq!(lsp.check(&[f], false).await.as_deref(), Some("m.py: clean now"));
    }
}
