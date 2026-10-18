//! Script extensions (L7.2 tier 1): discovery, loading with a stripped-source
//! cache, tools, commands, hooks and event handlers. A crashing or hung
//! extension is disabled with a notice; the session carries on.

use crate::hooks::{Gate, Handler};
use futures_util::future::BoxFuture;
use novacode_script::ext::{ExtHost, Extension, Failure};
use novacode_tools::{Exposure, Tool, ToolCtx, ToolOutput};
use novacode_types::{AgentEvent, Block, StreamEvent};
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

fn fnv(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in bytes {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// TypeScript stripped once per source version (L7.2 compile-on-change cache).
fn compiled(src: &str, cache: &Path) -> String {
    let key = format!("{:016x}-{}", fnv(src.as_bytes()), env!("CARGO_PKG_VERSION"));
    let path = cache.join(format!("{key}.js"));
    if let Ok(js) = std::fs::read_to_string(&path) {
        return js;
    }
    let js = novacode_script::ts::to_js(src, true);
    let _ = std::fs::create_dir_all(cache);
    let _ = novacode_base::storage::write_atomic(&path, js.as_bytes(), novacode_base::storage::Durability::Fast);
    js
}

/// The host side of `novacode.exec/readFile/writeFile/notify/log`.
struct Host {
    name: String,
    cwd: PathBuf,
    home: PathBuf,
    events: Arc<OnceLock<broadcast::Sender<AgentEvent>>>,
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
        let (name, cwd, home, events) = (
            self.name.clone(),
            self.cwd.clone(),
            self.home.clone(),
            self.events.clone(),
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
                        &events,
                        format!("[{name}] {}", args["text"].as_str().unwrap_or_default()),
                    );
                    Ok(Value::Null)
                }
                "log" => {
                    tracing::info!(extension = %name, "{}", args["text"].as_str().unwrap_or_default());
                    Ok(Value::Null)
                }
                _ => Err(format!("unknown operation `{op}`")),
            }
        })
    }
}

/// One loaded extension plus its health.
pub struct Loaded {
    pub ext: Arc<Extension>,
    pub path: PathBuf,
    off: AtomicBool,
    throws: AtomicU32,
}

/// Every extension of a session.
pub struct Extensions {
    pub loaded: Vec<Arc<Loaded>>,
    /// (name, error) of extensions that failed to load.
    pub failed: Vec<(String, String)>,
    events: Arc<OnceLock<broadcast::Sender<AgentEvent>>>,
}

impl Extensions {
    pub async fn load(dirs: &[PathBuf], cwd: &Path, home: &Path, cache: &Path) -> Arc<Extensions> {
        let events: Arc<OnceLock<broadcast::Sender<AgentEvent>>> = Arc::default();
        let mut loaded = Vec::new();
        let mut failed = Vec::new();
        for (name, path) in discover(dirs) {
            let src = match std::fs::read_to_string(&path) {
                Ok(s) => s,
                Err(e) => {
                    failed.push((name, e.to_string()));
                    continue;
                }
            };
            let host = Arc::new(Host {
                name: name.clone(),
                cwd: cwd.into(),
                home: home.into(),
                events: events.clone(),
            });
            match Extension::load(&name, compiled(&src, cache), host, 64 << 20).await {
                Ok(ext) => loaded.push(Arc::new(Loaded {
                    ext,
                    path,
                    off: AtomicBool::new(false),
                    throws: AtomicU32::new(0),
                })),
                Err(e) => failed.push((name, e)),
            }
        }
        Arc::new(Extensions { loaded, failed, events })
    }

    /// Route notices (disabled extensions, `novacode.notify`) to the session's events.
    pub fn attach(&self, tx: broadcast::Sender<AgentEvent>) {
        let _ = self.events.set(tx);
        for (n, e) in &self.failed {
            notice(&self.events, format!("extension `{n}` failed to load: {e}"));
        }
    }

    pub fn is_empty(&self) -> bool {
        self.loaded.is_empty()
    }

    /// Run one handler, turning crashes into a disabled extension and a notice.
    async fn invoke(&self, l: &Loaded, handler: u32, payload: &Value, timeout: Duration) -> Result<Value, String> {
        if l.off.load(Ordering::Relaxed) {
            return Err(format!("extension `{}` is disabled", l.ext.name));
        }
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
                &self.events,
                format!("extension `{}` disabled: {why} (fix it and /reload)", l.ext.name),
            );
        }
    }

    pub fn tools(self: &Arc<Self>) -> Vec<Arc<dyn Tool>> {
        let mut out: Vec<Arc<dyn Tool>> = Vec::new();
        for l in &self.loaded {
            for t in &l.ext.reg.tools {
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
                    .reg
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
            .find_map(|l| l.ext.reg.commands.iter().find(|c| c.name == name).map(|c| (l, c)))?;
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
                .reg
                .events
                .iter()
                .filter(move |(e, _)| e == event)
                .map(move |(_, h)| (l, *h))
        })
    }

    /// Forward session events to `on(type, …)` handlers (observe-only), until the session ends.
    pub fn spawn_events(self: &Arc<Self>, mut rx: broadcast::Receiver<AgentEvent>) {
        let me = self.clone();
        let wanted: Vec<String> = me
            .loaded
            .iter()
            .flat_map(|l| l.ext.reg.events.iter().map(|(e, _)| e.clone()))
            .collect();
        if wanted.iter().all(|e| e == "before_tool" || e == "after_tool") {
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
                    .invoke(l, h, &json!({ "tool": tool, "args": args }), HOOK_TIMEOUT)
                    .await
                else {
                    continue;
                };
                if let Some(why) = r["block"].as_str() {
                    return Gate::Block(format!("{} ({})", why, l.ext.name));
                }
                if r["args"].is_object() {
                    args = r["args"].clone();
                    changed = true;
                }
            }
            if changed { Gate::Args(args) } else { Gate::Allow }
        })
    }

    fn after_tool<'a>(&'a self, tool: &'a str, args: &'a Value, out: &'a ToolOutput) -> BoxFuture<'a, Option<String>> {
        Box::pin(async move {
            let handlers: Vec<_> = self.handlers("after_tool").collect();
            if handlers.is_empty() {
                return None;
            }
            let text = text_of(out);
            let payload = json!({ "tool": tool, "args": args, "isError": out.is_error, "output": text.chars().take(16_000).collect::<String>() });
            let mut fb = Vec::new();
            for (l, h) in handlers {
                if let Ok(Value::String(s)) = self.invoke(l, h, &payload, HOOK_TIMEOUT).await {
                    fb.push(s);
                }
            }
            (!fb.is_empty()).then(|| fb.join("\n"))
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
        exts.attach(tx);
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
            let l = exts2.loaded.iter().find(|l| l.ext.name == "multi").unwrap();
            exts2
                .invoke(l, l.ext.reg.tools[0].handler, &json!({}), Duration::from_millis(300))
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
