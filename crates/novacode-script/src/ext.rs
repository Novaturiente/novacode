//! Script extensions (L7.2 tier 1): one long-lived QuickJS runtime per extension,
//! on its own thread. The extension registers tools, commands and event handlers
//! through the `novacode` object; the core invokes them by handler id.
//!
//! API v1 (the object passed to `export default function (novacode) {…}`, also
//! the global `novacode`):
//! - `registerTool({ name, description, parameters, exposure?, readOnly?, execute(args, ctx) })`
//! - `registerCommand(name, { description, handler(args) })`
//! - `on(event, handler)`: `before_tool` (return `{ block }` or `{ args }`),
//!   `after_tool` (return feedback text), or any session event type (observe).
//! - `exec(command, { timeout? })`, `fs.read(path)`, `fs.write(path, text)`, `session.messages()`, `complete(req)`, `spawnAgent(task, opts)`,
//!   `notify(text)`, `remind(text, {wake})`, `log(...)`, `apiVersion`.
//!   Events include `input` and the tail-only `before_request`.

use futures_util::future::BoxFuture;
use rquickjs::{
    AsyncContext, AsyncRuntime, CatchResultExt, Function, Module, Object, Promise, Value as JsValue, prelude::Async,
};
use serde_json::{Value, json};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, oneshot};

pub const API_VERSION: u32 = 1;

/// What the extension's `novacode.*` calls reach on the host side.
pub trait ExtHost: Send + Sync + 'static {
    /// `op` is `exec`, `readFile`, `writeFile`, `notify`, `remind`, `log`, `ui`, `ask`, `messages`, `complete` or `spawnAgent`.
    fn call(&self, op: String, args: Value) -> BoxFuture<'static, Result<Value, String>>;
}

#[derive(Debug, Clone)]
pub struct ToolReg {
    pub name: String,
    pub description: String,
    pub schema: Value,
    pub exposure: Option<String>,
    /// Declared to only read: runs in read-only mode.
    pub read_only: bool,
    pub handler: u32,
}

#[derive(Debug, Clone)]
pub struct CommandReg {
    pub name: String,
    pub description: String,
    pub handler: u32,
}

#[derive(Debug, Clone, Default)]
pub struct Registration {
    pub tools: Vec<ToolReg>,
    pub commands: Vec<CommandReg>,
    /// (event, handler id), in registration order.
    pub events: Vec<(String, u32)>,
}

/// How a failed invocation went: the extension stays usable only after `Threw`.
#[derive(Debug, Clone, PartialEq)]
pub enum Failure {
    /// The handler threw or rejected; the message.
    Threw(String),
    /// Hung past its deadline, ran out of memory, or the thread is gone.
    Fatal(String),
}

#[derive(Debug)]
struct Req {
    handler: u32,
    payload: String,
    timeout: Duration,
    reply: oneshot::Sender<Result<Value, Failure>>,
}

#[derive(Debug)]
pub struct Extension {
    pub name: String,
    pub reg: Registration,
    tx: mpsc::UnboundedSender<Req>,
}

const PRELUDE: &str = r#"
((bridge, version) => {
  const handlers = [];
  const shortcuts = new Map();
  const add = (fn, what) => { if (typeof fn !== "function") throw new TypeError(`${what} must be a function`); handlers.push(fn); return handlers.length - 1; };
  const call = async (op, args) => { const r = JSON.parse(await bridge.host(op, JSON.stringify(args ?? null))); if (!r.ok) throw new Error(`${op}: ${r.error}`); return r.value; };
  const fmt = (v) => typeof v === "string" ? v : (() => { try { return JSON.stringify(v); } catch (_) { return String(v); } })();
  const api = Object.freeze({
    apiVersion: version,
    registerTool(t) {
      if (!t || typeof t.name !== "string") throw new TypeError("registerTool: name is required");
      const run = t.execute;
      if (typeof run !== "function") throw new TypeError("execute must be a function");
      const handler = add((p) => run(p.args, { cwd: p.cwd, session: p.session }), "execute");
      bridge.reg("tool", JSON.stringify({ name: t.name, description: String(t.description ?? ""), schema: t.parameters ?? { type: "object", properties: {} }, exposure: t.exposure ?? null, readOnly: t.readOnly === true, handler }));
    },
    registerCommand(name, c) {
      bridge.reg("command", JSON.stringify({ name: String(name).replace(/^\//, ""), description: String(c?.description ?? ""), handler: add(c?.handler, "handler") }));
    },
    on(event, fn) { bridge.reg("event", JSON.stringify({ event: String(event), handler: add(fn, "handler") })); },
    exec: (command, opts) => call("exec", { command, timeout: opts?.timeout }),
    fs: Object.freeze({
      read: (path) => call("readFile", { path }),
      write: (path, text) => call("writeFile", { path, text }),
    }),
    session: Object.freeze({
      // The conversation as of the last finished run, without the system prompt.
      messages: () => call("messages"),
    }),
    // One model call on the session's provider and login: {messages: [{role, content}], system?, maxTokens?}.
    complete: (req) => call("complete", req),
    // A subagent through the agent tool (same limits); resolves to {id}.
    spawnAgent: (task, opts) => call("spawnAgent", { task, opts: opts ?? {} }),
    // A real user message: `deliverAs` "steer" (into the running turn) or "followUp" (after it, or now when idle).
    sendMessage: (text, opts) => call("sendMessage", { text: fmt(text), deliverAs: opts?.deliverAs ?? "followUp" }),
    settings: Object.freeze({
      // Merged settings at `path` (dotted; omit for all). `extensions.<your name>` is your own block.
      get: (path) => call("settings", { path: path ?? null }),
    }),
    // Entries kept in the session file (never sent to the model), on the current branch.
    state: Object.freeze({
      append: (kind, data) => call("stateAppend", { kind: String(kind), data: data ?? null }),
      entries: (kind) => call("stateEntries", { kind: String(kind) }),
    }),
    notify: (text) => call("notify", { text: fmt(text) }),
    // A note for the model, saved and sent at the next model call; `wake` starts a run when idle.
    remind: (text, opts) => call("remind", { text: fmt(text), wake: !!(opts && opts.wake) }),
    log: (...a) => { bridge.host("log", JSON.stringify({ text: a.map(fmt).join(" ") })); },
    // UI API v1 (L9): data in, novacode draws it. Keys come back as on("ui_key").
    ui: Object.freeze({
      version: 1,
      // Text, or styled segments like every other UI call (joined; the first segment's style wins).
      setStatus: (key, text, opts) => {
        let style = opts?.style ?? null;
        if (Array.isArray(text)) {
          style = style ?? text.find((s) => s && s.style)?.style ?? null;
          text = text.map((s) => (typeof s === "string" ? s : s?.text ?? "")).join("");
        }
        return call("ui", { op: "status", key, text: text == null ? null : fmt(text), style });
      },
      setWidget: (key, lines, opts) => call("ui", { op: "widget", key, lines: lines ?? null, below: !!opts?.below }),
      setFooter: (lines) => call("ui", { op: "footer", lines: lines ?? null }),
      setHeader: (lines) => call("ui", { op: "header", lines: lines ?? null }),
      // Dialogs, in the ask_user form; `undefined` when dismissed or with no window.
      ask: (questions) => call("ask", { questions }),
      select: async (question, options) => {
        const r = await call("ask", { questions: [{ question, options: options.map((o) => typeof o === "string" ? { label: o } : o) }] });
        return r ? (r[0].chosen[0] ?? r[0].text ?? undefined) : undefined;
      },
      confirm: async (question) => {
        const r = await call("ask", { questions: [{ question, options: [{ label: "Yes" }, { label: "No" }] }] });
        return r ? r[0].chosen[0] === "Yes" : undefined;
      },
      input: async (question) => {
        const r = await call("ask", { questions: [{ question, options: [] }] });
        return r ? (r[0].text ?? r[0].chosen[0] ?? "") : undefined;
      },
      setEditor: (d) => call("ui", { op: "editor", title: d?.title ?? null, border: d?.border ?? null }),
      openTab: (name, o) => call("ui", { op: "open_tab", name, title: o?.title ?? null, priority: o?.priority ?? 0, width: o?.width ?? null, region: !!o?.region, show: !!o?.show }),
      showTab: (name) => call("ui", { op: "show_tab", name }),
      closeTab: (name) => call("ui", { op: "close_tab", name }),
      setSource: (tab, id, o) => call("ui", { op: "source", tab, id, title: o?.title ?? null, order: o?.order ?? 0, newest_first: !!o?.newestFirst }),
      publish: (tab, source, node) => call("ui", { op: "publish", tab, source, node }),
      unpublish: (tab, source, id) => call("ui", { op: "unpublish", tab, source, id: id ?? null }),
      setRegion: (tab, lines) => call("ui", { op: "region", tab, lines }),
    }),
    registerShortcut(key, s) {
      const fn = typeof s === "function" ? s : s?.handler;
      if (typeof fn !== "function") throw new TypeError("registerShortcut(key, handler)");
      if (!shortcuts.size) api.on("ui_key", (e) => (e.tab == null && shortcuts.has(e.key) ? shortcuts.get(e.key)(e) : undefined));
      shortcuts.set(String(key), fn);
      call("ui", { op: "shortcut", key: String(key), description: String(s?.description ?? "") });
    },
  });
  globalThis.novacode = api;
  globalThis.console = { log: api.log, info: api.log, warn: api.log, error: api.log, debug: api.log };
  globalThis.__dispatch = async (id, payload) => {
    const r = await handlers[id](JSON.parse(payload));
    return r === undefined ? "null" : JSON.stringify(r);
  };
  return api;
})
"#;

/// Anything that registered tools/commands/handlers and runs them by id: a
/// script extension here, a process extension in the core (L7.2 tier 2).
pub trait Backend: Send + Sync {
    fn name(&self) -> &str;
    /// Its own event-handler timeout (a process extension's `initialize` may ask for one).
    fn hook_timeout(&self) -> Option<Duration> {
        None
    }
    fn reg(&self) -> &Registration;
    fn invoke<'a>(
        &'a self,
        handler: u32,
        payload: &'a Value,
        timeout: Duration,
    ) -> BoxFuture<'a, Result<Value, Failure>>;
}

impl Backend for Extension {
    fn name(&self) -> &str {
        &self.name
    }
    fn reg(&self) -> &Registration {
        &self.reg
    }
    fn invoke<'a>(
        &'a self,
        handler: u32,
        payload: &'a Value,
        timeout: Duration,
    ) -> BoxFuture<'a, Result<Value, Failure>> {
        Box::pin(Extension::invoke(self, handler, payload, timeout))
    }
}

impl Extension {
    /// Load `js` (already TypeScript-stripped, an ES module) and run its setup.
    /// `imports` = (entry file, strip cache): relative imports resolve inside the
    /// entry's folder (L7.10). `None` = no imports.
    pub async fn load(
        name: &str,
        js: String,
        host: Arc<dyn ExtHost>,
        memory_bytes: usize,
        imports: Option<(PathBuf, PathBuf)>,
    ) -> Result<Arc<Extension>, String> {
        let (tx, rx) = mpsc::unbounded_channel::<Req>();
        let (ltx, lrx) = oneshot::channel();
        let outer = tokio::runtime::Handle::current();
        let n = name.to_string();
        std::thread::Builder::new()
            .name(format!("ext-{name}"))
            .spawn(move || {
                let rt = match tokio::runtime::Builder::new_current_thread().enable_time().build() {
                    Ok(rt) => rt,
                    Err(e) => {
                        let _ = ltx.send(Err(e.to_string()));
                        return;
                    }
                };
                rt.block_on(thread(n, js, host, outer, memory_bytes, imports, ltx, rx));
            })
            .map_err(|e| e.to_string())?;
        let reg = tokio::time::timeout(Duration::from_secs(10), lrx)
            .await
            .map_err(|_| "setup took longer than 10s".to_string())?
            .map_err(|_| "the extension thread died during setup".to_string())??;
        Ok(Arc::new(Extension {
            name: name.into(),
            reg,
            tx,
        }))
    }

    /// Run handler `id` with `payload`; at most `timeout`.
    pub async fn invoke(&self, handler: u32, payload: &Value, timeout: Duration) -> Result<Value, Failure> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(Req {
                handler,
                payload: payload.to_string(),
                timeout,
                reply,
            })
            .map_err(|_| Failure::Fatal("the extension has stopped".into()))?;
        // The thread enforces the deadline; this guards against it being stuck elsewhere.
        match tokio::time::timeout(timeout + Duration::from_secs(5), rx).await {
            Ok(Ok(r)) => r,
            Ok(Err(_)) => Err(Failure::Fatal("the extension thread died".into())),
            Err(_) => Err(Failure::Fatal(format!("no answer within {}s", timeout.as_secs()))),
        }
    }
}

struct Shared {
    host: Arc<dyn ExtHost>,
    outer: tokio::runtime::Handle,
    reg: Mutex<Registration>,
    /// Deadline in ms since `start` for the running call; 0 = none.
    deadline: AtomicU64,
    start: Instant,
    stopped: AtomicBool,
}

#[allow(clippy::too_many_arguments)] // private thread entry, one caller
async fn thread(
    name: String,
    js: String,
    host: Arc<dyn ExtHost>,
    outer: tokio::runtime::Handle,
    memory_bytes: usize,
    imports: Option<(PathBuf, PathBuf)>,
    loaded: oneshot::Sender<Result<Registration, String>>,
    mut rx: mpsc::UnboundedReceiver<Req>,
) {
    let shared = Arc::new(Shared {
        host,
        outer,
        reg: Mutex::default(),
        deadline: AtomicU64::new(0),
        start: Instant::now(),
        stopped: AtomicBool::new(false),
    });
    let setup = async {
        let rt = AsyncRuntime::new().map_err(|e| e.to_string())?;
        rt.set_memory_limit(memory_bytes).await;
        rt.set_max_stack_size(1 << 20).await;
        let s = shared.clone();
        rt.set_interrupt_handler(Some(Box::new(move || {
            let d = s.deadline.load(Ordering::Relaxed);
            d != 0 && s.start.elapsed().as_millis() as u64 > d
        })))
        .await;
        // The entry's module name is its real path, so imports resolve next to it.
        let file = match &imports {
            Some((entry, cache)) => {
                let entry = std::fs::canonicalize(entry).map_err(|e| e.to_string())?;
                let root = entry.parent().unwrap_or(Path::new("/")).to_path_buf();
                rt.set_loader(Imports { root: root.clone() }, Imports { root: cache.clone() })
                    .await;
                entry.display().to_string()
            }
            None => format!("{name}.js"),
        };
        let ctx = AsyncContext::full(&rt).await.map_err(|e| e.to_string())?;
        // Setup gets 5s of JS time.
        shared
            .deadline
            .store(shared.start.elapsed().as_millis() as u64 + 5000, Ordering::Relaxed);
        let r = ctx
            .async_with(async |ctx| -> Result<(), String> {
                let bridge = bridge(&ctx, &shared).map_err(|e| e.to_string())?;
                let prelude: Function = ctx.eval(PRELUDE).catch(&ctx).map_err(|e| e.to_string())?;
                let api: Object = prelude
                    .call((bridge, API_VERSION))
                    .catch(&ctx)
                    .map_err(|e| e.to_string())?;
                let (module, promise) = Module::declare(ctx.clone(), file, js)
                    .catch(&ctx)
                    .map_err(|e| e.to_string())?
                    .eval()
                    .catch(&ctx)
                    .map_err(|e| e.to_string())?;
                promise
                    .into_future::<()>()
                    .await
                    .catch(&ctx)
                    .map_err(|e| e.to_string())?;
                if let Ok(f) = module.get::<_, Function>("default") {
                    let r: JsValue = f.call((api,)).catch(&ctx).map_err(|e| e.to_string())?;
                    if let Some(p) = r.as_promise() {
                        p.clone()
                            .into_future::<()>()
                            .await
                            .catch(&ctx)
                            .map_err(|e| e.to_string())?;
                    }
                }
                Ok(())
            })
            .await;
        // Calls the setup didn't await (`nc.ui.setStatus(…)` without `await`) still run.
        if r.is_ok() && tokio::time::timeout(Duration::from_secs(5), rt.idle()).await.is_err() {
            return Err("setup left work running for more than 5s".into());
        }
        shared.deadline.store(0, Ordering::Relaxed);
        r.map(|_| (rt, ctx))
    };
    let (rt, ctx) = match setup.await {
        Ok(v) => v,
        Err(e) => {
            let _ = loaded.send(Err(e));
            return;
        }
    };
    let _ = loaded.send(Ok(shared.reg.lock().unwrap().clone()));
    while let Some(req) = rx.recv().await {
        if shared.stopped.load(Ordering::Relaxed) {
            let _ = req.reply.send(Err(Failure::Fatal("the extension is disabled".into())));
            continue;
        }
        shared.deadline.store(
            shared.start.elapsed().as_millis() as u64 + req.timeout.as_millis() as u64,
            Ordering::Relaxed,
        );
        let call = ctx.async_with(async |ctx| -> Result<Value, Failure> {
            let f: Function = ctx
                .globals()
                .get("__dispatch")
                .map_err(|e| Failure::Fatal(e.to_string()))?;
            let p: Promise = f
                .call((req.handler, req.payload.clone()))
                .catch(&ctx)
                .map_err(|e| classify(&e.to_string()))?;
            let s: String = p
                .into_future::<String>()
                .await
                .catch(&ctx)
                .map_err(|e| classify(&e.to_string()))?;
            Ok(serde_json::from_str(&s).unwrap_or(Value::Null))
        });
        let r = match tokio::time::timeout(req.timeout, async {
            let r = call.await;
            // Finish what the handler started but didn't await.
            rt.idle().await;
            r
        })
        .await
        {
            Ok(r) => r,
            Err(_) => Err(Failure::Fatal(format!("hung for more than {}s", req.timeout.as_secs()))),
        };
        shared.deadline.store(0, Ordering::Relaxed);
        if matches!(r, Err(Failure::Fatal(_))) {
            shared.stopped.store(true, Ordering::Relaxed);
        }
        let _ = req.reply.send(r);
    }
    drop(ctx);
    drop(rt);
}

/// Interrupts (deadline) and out-of-memory leave the extension unusable.
fn classify(e: &str) -> Failure {
    if e.contains("interrupted") {
        Failure::Fatal("hung past its deadline".into())
    } else if e.contains("out of memory") {
        Failure::Fatal("ran out of memory".into())
    } else {
        Failure::Threw(e.trim().to_string())
    }
}

fn bridge<'js>(ctx: &rquickjs::Ctx<'js>, shared: &Arc<Shared>) -> rquickjs::Result<Object<'js>> {
    let b = Object::new(ctx.clone())?;
    let s = shared.clone();
    b.set(
        "reg",
        Function::new(ctx.clone(), move |kind: String, raw: String| -> rquickjs::Result<()> {
            let v: Value = serde_json::from_str(&raw).unwrap_or_default();
            let id = v["handler"].as_u64().unwrap_or_default() as u32;
            let mut r = s.reg.lock().unwrap();
            match kind.as_str() {
                "tool" => r.tools.push(ToolReg {
                    name: v["name"].as_str().unwrap_or_default().into(),
                    description: v["description"].as_str().unwrap_or_default().into(),
                    schema: v["schema"].clone(),
                    exposure: v["exposure"].as_str().map(String::from),
                    read_only: v["readOnly"] == true,
                    handler: id,
                }),
                "command" => r.commands.push(CommandReg {
                    name: v["name"].as_str().unwrap_or_default().into(),
                    description: v["description"].as_str().unwrap_or_default().into(),
                    handler: id,
                }),
                _ => r.events.push((v["event"].as_str().unwrap_or_default().into(), id)),
            }
            Ok(())
        })?,
    )?;
    let s = shared.clone();
    b.set(
        "host",
        Function::new(
            ctx.clone(),
            Async(move |op: String, raw: String| {
                let s = s.clone();
                async move {
                    let args: Value = serde_json::from_str(&raw).unwrap_or(Value::Null);
                    let fut = s.host.call(op, args);
                    let r = s.outer.spawn(fut).await.unwrap_or_else(|e| Err(e.to_string()));
                    Ok::<String, rquickjs::Error>(match r {
                        Ok(v) => json!({ "ok": true, "value": v }).to_string(),
                        Err(e) => json!({ "ok": false, "error": e }).to_string(),
                    })
                }
            }),
        )?,
    )?;
    Ok(b)
}

/// Relative imports (L7.10). As the resolver, `root` is the extension's folder and
/// a resolved file must stay inside it; as the loader, `root` is the strip cache.
/// No bare names: npm packages need Node built-ins QuickJS lacks (use a process extension).
struct Imports {
    root: PathBuf,
}

impl rquickjs::loader::Resolver for Imports {
    fn resolve<'js>(
        &mut self,
        _ctx: &rquickjs::Ctx<'js>,
        base: &str,
        name: &str,
        _attrs: Option<rquickjs::loader::ImportAttributes<'js>>,
    ) -> rquickjs::Result<String> {
        let fail = |why: &str| rquickjs::Error::new_resolving_message(base, name, why.to_string());
        if !name.starts_with("./") && !name.starts_with("../") {
            return Err(fail("only relative imports (./ or ../) inside the extension's folder"));
        }
        let mut p = Path::new(base).parent().unwrap_or(Path::new("/")).to_path_buf();
        for c in Path::new(name).components() {
            match c {
                Component::ParentDir => {
                    p.pop();
                }
                Component::Normal(s) => p.push(s),
                _ => {}
            }
        }
        let candidates = [
            p.clone(),
            p.with_extension("ts"),
            p.with_extension("js"),
            p.join("index.ts"),
            p.join("index.js"),
        ];
        let found = candidates
            .iter()
            .find(|c| c.is_file())
            .ok_or_else(|| fail("file not found"))?;
        let real = std::fs::canonicalize(found).map_err(|e| fail(&e.to_string()))?;
        if !real.starts_with(&self.root) {
            return Err(fail("outside the extension's folder"));
        }
        Ok(real.display().to_string())
    }
}

impl rquickjs::loader::Loader for Imports {
    fn load<'js>(
        &mut self,
        ctx: &rquickjs::Ctx<'js>,
        name: &str,
        _attrs: Option<rquickjs::loader::ImportAttributes<'js>>,
    ) -> rquickjs::Result<Module<'js, rquickjs::module::Declared>> {
        let src =
            std::fs::read_to_string(name).map_err(|e| rquickjs::Error::new_loading_message(name, e.to_string()))?;
        Module::declare(ctx.clone(), name, crate::ts::cached(&src, &self.root))
    }
}

#[cfg(test)]
mod tests {

    /// docs/extensions.md lists every function of the API objects (L11.8).
    #[test]
    fn docs_cover_the_api() {
        let doc = include_str!("../../../docs/extensions.md");
        let api = PRELUDE.split_once("const api = Object.freeze({").unwrap().1;
        let mut names = Vec::new();
        for l in api.lines() {
            let t = l.trim_start();
            let indent = l.len() - t.len();
            if indent != 4 && indent != 6 {
                continue;
            }
            let name: String = t.chars().take_while(|c| c.is_ascii_alphanumeric()).collect();
            let rest = &t[name.len()..];
            if !name.is_empty()
                && ((rest.starts_with('(') && t.ends_with('{'))
                    || rest.starts_with(": (")
                    || rest.starts_with(": async"))
            {
                names.push(name);
            }
        }
        assert!(names.len() > 20, "{names:?}");
        let missing: Vec<_> = names.iter().filter(|n| !doc.contains(&format!("{n}("))).collect();
        assert!(missing.is_empty(), "undocumented in docs/extensions.md: {missing:?}");
    }

    use super::*;

    struct Host(Mutex<Vec<String>>);
    impl ExtHost for Host {
        fn call(&self, op: String, args: Value) -> BoxFuture<'static, Result<Value, String>> {
            self.0.lock().unwrap().push(format!("{op} {args}"));
            Box::pin(async move {
                if op == "exec" {
                    Ok(json!({"code": 0, "stdout": "hi\n"}))
                } else {
                    Ok(Value::Null)
                }
            })
        }
    }

    async fn load(js: &str) -> Result<Arc<Extension>, String> {
        Extension::load(
            "t",
            crate::ts::to_js(js, true),
            Arc::new(Host(Mutex::default())),
            32 << 20,
            None,
        )
        .await
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn registers_and_invokes() {
        let e = load(
            "export default function (nc: any) {\n\
               nc.registerTool({ name: 'greet', description: 'Say hi', parameters: { type: 'object', properties: { who: { type: 'string' } } },\n\
                 execute: async (a: { who: string }) => { const r = await nc.exec('echo hi'); return `${r.stdout.trim()} ${a.who}`; } });\n\
               nc.registerCommand('/stats', { description: 'show', handler: () => 'ok' });\n\
               nc.on('before_tool', (ev) => ev.tool === 'bash' ? { block: 'no bash' } : undefined);\n\
               nc.on('loop', () => { while (true) {} });\n\
               nc.on('throw', () => { throw new Error('bad'); });\n\
             }",
        )
        .await
        .unwrap();
        assert_eq!(e.reg.tools[0].name, "greet");
        assert_eq!(e.reg.commands[0].name, "stats");
        let t = Duration::from_secs(2);
        assert_eq!(
            e.invoke(e.reg.tools[0].handler, &json!({"args": {"who": "nova"}}), t)
                .await,
            Ok(json!("hi nova"))
        );
        assert_eq!(
            e.invoke(e.reg.events[0].1, &json!({"tool": "bash"}), t).await,
            Ok(json!({"block": "no bash"}))
        );
        assert_eq!(
            e.invoke(e.reg.events[0].1, &json!({"tool": "read"}), t).await,
            Ok(Value::Null)
        );
        assert!(
            matches!(e.invoke(e.reg.events[2].1, &json!({}), t).await, Err(Failure::Threw(m)) if m.contains("bad"))
        );
        let r = e
            .invoke(e.reg.events[1].1, &json!({}), Duration::from_millis(300))
            .await;
        assert!(matches!(r, Err(Failure::Fatal(_))), "{r:?}");
        assert!(
            matches!(
                e.invoke(e.reg.commands[0].handler, &json!({}), t).await,
                Err(Failure::Fatal(_))
            ),
            "stays disabled"
        );
    }

    /// L11.12 budget: one hook round trip < 1 ms. Run by `scripts/bench.sh` (`--ignored`).
    #[tokio::test(flavor = "multi_thread")]
    #[ignore]
    async fn bench_hook() {
        let e = load(
            "export default (nc) => nc.on('before_tool', (ev) => ev.tool === 'bash' ? { block: 'no' } : undefined)",
        )
        .await
        .unwrap();
        let (h, t) = (e.reg.events[0].1, Duration::from_secs(2));
        let payload = json!({"tool": "read", "args": {"path": "src/main.rs"}, "cwd": "/tmp"});
        for _ in 0..100 {
            e.invoke(h, &payload, t).await.unwrap();
        }
        let n = 5000;
        let start = std::time::Instant::now();
        for _ in 0..n {
            e.invoke(h, &payload, t).await.unwrap();
        }
        let us = start.elapsed().as_secs_f64() * 1e6 / n as f64;
        println!("BENCH hook_us {us:.1}");
        assert!(us < 1000.0, "hook {us:.1} µs over the 1 ms budget");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn bad_setup_is_an_error() {
        assert!(load("throw new Error('nope')").await.unwrap_err().contains("nope"));
        assert!(load("while (true) {}").await.unwrap_err().contains("interrupted"));
        assert!(
            load("export default (nc) => nc.registerTool({ name: 'x' })")
                .await
                .unwrap_err()
                .contains("execute")
        );
    }
}
