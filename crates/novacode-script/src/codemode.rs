//! Codemode engine (L7.5): a fresh QuickJS context per script, on its own thread.
//! The script reaches the outside world only through [`Host::call`], which the
//! core routes through the normal tool pipeline (permissions, hooks, guards).

use futures_util::future::BoxFuture;
use rquickjs::{AsyncContext, AsyncRuntime, CatchResultExt, Function, Promise, Value as JsValue, prelude::Async};
use serde_json::{Map, Value, json};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Something a script can call: `name` is the tool's real name (`server__tool` for MCP).
pub trait Host: Send + Sync + 'static {
    fn call(&self, name: String, args: Value) -> BoxFuture<'static, Result<Value, String>>;
    /// Time this script's calls have spent waiting on the user so far (approval
    /// cards, `ask_user`), including a wait still going; it doesn't count toward the timeout.
    fn user_wait(&self) -> Duration {
        Duration::ZERO
    }
}

#[derive(Clone, Debug)]
pub struct ToolInfo {
    pub name: String,
    pub description: String,
    pub schema: Value,
    /// MCP server for `server__tool` names; reached as `mcp.<server>.<tool>()`.
    pub namespace: Option<String>,
    /// JSON schema of the structured result (MCP `outputSchema`); `None` = text.
    pub output_schema: Option<Value>,
}

#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub memory_bytes: usize,
    pub timeout: Duration,
    pub max_calls: usize,
    pub max_output_bytes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            memory_bytes: 64 << 20,
            timeout: Duration::from_secs(300),
            max_calls: 500,
            max_output_bytes: 16 << 20,
        }
    }
}

/// What the model sees: text, console lines and images, in order.
#[derive(Clone, Debug, PartialEq)]
pub enum Item {
    Text(String),
    Console(String),
    Image { data: String, mime: String },
}

#[derive(Debug, Default)]
pub struct Outcome {
    pub items: Vec<Item>,
    /// The script's `return` value (undefined → None).
    pub result: Option<Value>,
    pub error: Option<String>,
    pub calls: usize,
    /// The whole store after the run.
    pub store: Map<String, Value>,
    pub elapsed: Duration,
    pub truncated: bool,
}

/// Most a script's store may hold, as JSON (L7.17).
const STORE_CAP: usize = 8 << 20;

const PRELUDE: &str = r#"
((bridge) => {
  const stringify = JSON.stringify, parse = JSON.parse;
  const fmt = (v) => {
    if (typeof v === "string") return v;
    if (v instanceof Error) return v.stack ? `${v.name}: ${v.message}\n${v.stack}` : `${v.name}: ${v.message}`;
    try { const s = stringify(v); return s === undefined ? String(v) : s; } catch (_) { return String(v); }
  };
  const log = (level) => (...a) => bridge.out(level === "log" || level === "info" ? "console" : "console", (level === "log" || level === "info" ? "" : `[${level}] `) + a.map(fmt).join(" "));
  globalThis.console = { log: log("log"), info: log("info"), warn: log("warn"), error: log("error"), debug: log("debug") };
  globalThis.text = (v) => { bridge.out("text", fmt(v)); };
  globalThis.image = (v) => {
    let data, mime;
    if (typeof v === "string" && v.startsWith("data:")) {
      const m = /^data:([^;,]+);base64,(.*)$/s.exec(v);
      if (!m) throw new TypeError("image(): expected a base64 data: URL");
      mime = m[1]; data = m[2];
    } else if (v && typeof v === "object" && v.type === "image") { data = v.data; mime = v.mimeType || v.mime; }
    else if (v && typeof v === "object" && typeof v.image_url === "string") return globalThis.image(v.image_url);
    if (!data || !mime) throw new TypeError('image(): expected a data: URL or { type: "image", data, mimeType }');
    bridge.img(String(data), String(mime));
  };
  const EXIT = Object.freeze({ exit: true });
  globalThis.exit = () => { throw EXIT; };
  globalThis.__EXIT = EXIT;
  // MCP tools are named as the script reaches them (`mcp.S.T`).
  const labels = {};
  const call = async (name, args) => {
    // Async frames are lost after the await, so note the script's line now.
    // The wrapper adds one line; name the model's own line (caught errors skip fix_lines).
    const m = /script\.js:(\d+)/.exec(new Error().stack || "");
    const site = m ? `line ${Math.max(1, Number(m[1]) - 1)}` : "";
    const r = parse(await bridge.call(String(name), stringify(args === undefined ? {} : args)));
    if (!r.ok) {
      // The site goes on the first line; a declaration may follow.
      const [head, ...rest] = String(r.error).split("\n");
      const e = new Error([`${labels[name] || name}: ${head}${site ? ` (at ${site})` : ""}`, ...rest].join("\n"));
      e.tool = name; throw e;
    }
    return r.value;
  };
  globalThis.callTool = call;
  const list = parse(bridge.tools());
  const names = new Set(list.map((t) => t.name));
  const unknown = (name) => { throw new Error(`unknown tool '${name}'. Use searchTools("…") to find tools.`); };
  globalThis.tools = new Proxy({}, {
    get(_, n) {
      if (typeof n !== "string" || n === "then") return undefined;
      // Scripts see deferred tools directly; `tools.call({name, arguments})` is the same call.
      if (n === "call") return async (a) => (names.has(a?.name) ? call(a.name, a.arguments ?? {}) : unknown(a?.name));
      return names.has(n) ? (a) => call(n, a) : (a) => Promise.reject(new Error(`unknown tool '${n}'. Use searchTools("…") to find tools.`)); },
    has(_, n) { return names.has(String(n)); },
    ownKeys() { return list.filter((t) => !t.namespace).map((t) => t.name); },
    getOwnPropertyDescriptor(_, n) { return names.has(String(n)) ? { enumerable: true, configurable: true } : undefined; },
  });
  const spaces = {};
  for (const t of list) if (t.namespace) {
    const short = t.name.slice(t.namespace.length + 2);
    (spaces[t.namespace] ||= {})[short] = (a) => call(t.name, a);
    labels[t.name] = `mcp.${t.namespace}.${short}`;
  }
  globalThis.mcp = new Proxy(spaces, { get(o, s) { if (typeof s !== "string" || s === "then") return undefined; if (!(s in o)) throw new Error(`unknown MCP server '${s}'. Servers: ${Object.keys(o).join(", ") || "none"}`); return o[s]; } });
  globalThis.ALL_TOOLS = list.map((t) => ({ name: t.namespace ? `mcp.${t.namespace}.${t.name.slice(t.namespace.length + 2)}` : t.name, description: t.description }));
  // Async like everything else a script awaits, so `.then`/`.catch` work too.
  globalThis.searchTools = async (q, opts) => parse(bridge.search(String(q ?? ""), Number((opts && opts.limit) || 10)));
  globalThis.describeTool = async (n) => { const r = bridge.describe(String(n)); return r === "" ? undefined : r; };
  globalThis.describeNamespace = async (s) => { const r = bridge.namespace(String(s)); return r === "" ? undefined : r; };
  // Non-chat models (L2.26): classifiers, through the `classify` tool. Errors come back
  // in stopReason/errorMessage, as in pi.
  if (names.has("classify")) {
    const spec = (m) => typeof m === "string" ? m : `${m.provider}/${m.id}`;
    const avail = async (type, provider) => type !== "classifier" ? [] : (await call("classify", { list: true })).filter((m) => !provider || m.provider === provider).map((m) => ({ ...m, type: "classifier" }));
    globalThis.models = Object.freeze({
      getAvailableOfType: avail,
      getModelsOfType: avail,
      getModelOfType: async (type, provider, id) => (await avail(type, provider)).find((m) => m.id === id),
      classify: (model, context) => call("classify", { ...context, model: spec(model) }),
    });
  }
  globalThis.store = (k, v) => { const e = bridge.set(String(k), stringify(v === undefined ? null : v)); if (e) throw new Error(e); return v; };
  // Missing in QuickJS; a JSON copy covers what scripts use it for.
  if (typeof globalThis.structuredClone !== "function") globalThis.structuredClone = (v) => (v === undefined ? undefined : parse(stringify(v)));
  globalThis.load = (k) => { const r = bridge.get(String(k)); return r === "" ? undefined : parse(r); };
})
"#;

struct Shared {
    host: Arc<dyn Host>,
    outer: tokio::runtime::Handle,
    tools: Vec<ToolInfo>,
    store: Mutex<Store>,
    items: Mutex<(Vec<Item>, usize, bool)>,
    calls: AtomicUsize,
    cancelled: Arc<AtomicBool>,
    limits: Limits,
}

/// The script's store, with each key's JSON size and their total, so `store()`
/// checks the cap without re-serialising every value.
struct Store {
    map: Map<String, Value>,
    sizes: std::collections::HashMap<String, usize>,
    total: usize,
}

impl Store {
    fn new(map: Map<String, Value>) -> Self {
        let sizes: std::collections::HashMap<String, usize> =
            map.iter().map(|(k, v)| (k.clone(), v.to_string().len())).collect();
        let total = sizes.values().sum();
        Store { map, sizes, total }
    }

    /// Stores `v` under `k`, or says why not.
    fn set(&mut self, k: String, v: Value) -> Result<(), String> {
        let size = v.to_string().len();
        let others = self.total - self.sizes.get(&k).copied().unwrap_or(0);
        if others + size > STORE_CAP {
            return Err(format!(
                "store is full (8 MiB): storing `{k}` ({size} bytes) would pass it; keep less, or store(key, null) to free a key"
            ));
        }
        self.total = others + size;
        self.sizes.insert(k.clone(), size);
        self.map.insert(k, v);
        Ok(())
    }
}

impl Shared {
    fn push(&self, item: Item) {
        let mut g = self.items.lock().unwrap_or_else(|e| e.into_inner());
        let size = match &item {
            Item::Text(t) | Item::Console(t) => t.len(),
            Item::Image { data, .. } => data.len(),
        };
        if g.2 || g.1 + size > self.limits.max_output_bytes {
            g.2 = true;
            return;
        }
        g.1 += size;
        g.0.push(item);
    }
}

/// The JS name a tool is reached by, for search results and errors.
pub(crate) fn js_name(t: &ToolInfo) -> String {
    match &t.namespace {
        Some(ns) => format!("mcp.{ns}.{}", &t.name[ns.len() + 2..]),
        None => format!("tools.{}", t.name),
    }
}

/// A short TypeScript-ish declaration from a JSON schema (L7.5 typed declarations).
pub fn declaration(t: &ToolInfo) -> String {
    fn ty(s: &Value) -> String {
        if let Some(e) = s["enum"].as_array() {
            return e.iter().map(|v| v.to_string()).collect::<Vec<_>>().join(" | ");
        }
        match s["type"].as_str() {
            Some("string") => "string".into(),
            Some("integer") | Some("number") => "number".into(),
            Some("boolean") => "boolean".into(),
            Some("array") => format!("{}[]", ty(&s["items"])),
            Some("object") if s["properties"].is_object() => obj(s),
            _ => "any".into(),
        }
    }
    fn obj(s: &Value) -> String {
        let req: Vec<&str> = s["required"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|v| v.as_str())
            .collect();
        let props = s["properties"].as_object().cloned().unwrap_or_default();
        let fields: Vec<String> = props
            .iter()
            .map(|(k, v)| {
                let doc = v["description"]
                    .as_str()
                    .map(|d| format!(" /** {} */", d.lines().next().unwrap_or("")))
                    .unwrap_or_default();
                format!(
                    "{k}{}: {}{doc}",
                    if req.contains(&k.as_str()) { "" } else { "?" },
                    ty(v)
                )
            })
            .collect();
        format!("{{ {} }}", fields.join("; "))
    }
    let first = t.description.lines().next().unwrap_or("");
    let ret = t.output_schema.as_ref().map(ty).unwrap_or_else(|| "string".into());
    format!(
        "/** {first} */\n{}(args: {}): Promise<{ret}>",
        js_name(t),
        obj(&t.schema)
    )
}

/// Word-overlap search over names and descriptions (codemode's `searchTools`).
fn search(tools: &[ToolInfo], q: &str, limit: usize) -> Value {
    let words: Vec<String> = q
        .to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(String::from)
        .collect();
    let mut hits: Vec<(usize, &ToolInfo)> = tools
        .iter()
        .map(|t| {
            let name = t.name.to_lowercase();
            let desc = t.description.to_lowercase();
            let s = words
                .iter()
                .map(|w| 3 * name.contains(w.as_str()) as usize + desc.contains(w.as_str()) as usize)
                .sum();
            (s, t)
        })
        .filter(|(s, _)| *s > 0 || words.is_empty())
        .collect();
    hits.sort_by_key(|h| std::cmp::Reverse(h.0));
    Value::Array(
        hits.into_iter()
            .take(limit.clamp(1, 50))
            .map(|(_, t)| json!({ "name": js_name(t), "description": t.description.lines().next().unwrap_or("") }))
            .collect(),
    )
}

struct CancelOnDrop(Arc<AtomicBool>);
impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

/// Run `code` (JS or TS, the body of an async function). `cancel` aborts it.
pub async fn run(
    code: &str,
    host: Arc<dyn Host>,
    tools: Vec<ToolInfo>,
    store: Map<String, Value>,
    limits: Limits,
    cancel: Arc<AtomicBool>,
) -> Outcome {
    let _guard = CancelOnDrop(cancel.clone());
    let shared = Arc::new(Shared {
        host,
        outer: tokio::runtime::Handle::current(),
        tools,
        store: Mutex::new(Store::new(store)),
        items: Mutex::new((Vec::new(), 0, false)),
        calls: AtomicUsize::new(0),
        cancelled: cancel,
        limits,
    });
    let body = crate::ts::to_js(&format!("(async () => {{\n{code}\n}})()"), false);
    let (tx, rx) = tokio::sync::oneshot::channel();
    let s = shared.clone();
    let spawned = std::thread::Builder::new()
        .name("novacode-codemode".into())
        .spawn(move || {
            let started = Instant::now();
            let out = match tokio::runtime::Builder::new_current_thread().enable_time().build() {
                Ok(rt) => rt.block_on(on_thread(body, s, started)),
                Err(e) => Err(format!("could not start the script runtime: {e}")),
            };
            let _ = tx.send((out, started.elapsed()));
        });
    let (out, elapsed) = match spawned {
        Ok(_) => rx
            .await
            .unwrap_or_else(|_| (Err("the script thread panicked".into()), Duration::ZERO)),
        Err(e) => (Err(format!("could not start the script thread: {e}")), Duration::ZERO),
    };
    let (items, _, truncated) = std::mem::take(&mut *shared.items.lock().unwrap_or_else(|e| e.into_inner()));
    let store = std::mem::take(&mut shared.store.lock().unwrap_or_else(|e| e.into_inner()).map);
    let (result, error) = match out {
        Ok(r) => (r, None),
        Err(e) => (None, Some(e)),
    };
    Outcome {
        items,
        result,
        error,
        calls: shared.calls.load(Ordering::SeqCst),
        store,
        elapsed,
        truncated,
    }
}

/// QuickJS reports `script.js:N`; the wrapper adds one line, so name the model's line.
fn fix_lines(raw: &str) -> String {
    let mut out = String::new();
    let mut rest = raw;
    while let Some(i) = rest.find("script.js:") {
        out.push_str(&rest[..i]);
        rest = &rest[i + "script.js:".len()..];
        let n: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
        rest = &rest[n.len()..];
        // Drop the column too.
        if let Some(r) = rest.strip_prefix(':') {
            rest = r.trim_start_matches(|c: char| c.is_ascii_digit());
        }
        out.push_str(&format!(
            "line {}",
            n.parse::<usize>().unwrap_or(1).saturating_sub(1).max(1)
        ));
    }
    out.push_str(rest);
    let out = out.trim().to_string();
    if out.contains("out of memory") {
        format!("{out} (memory limit reached)")
    } else {
        out
    }
}

async fn on_thread(body: String, shared: Arc<Shared>, started: Instant) -> Result<Option<Value>, String> {
    let rt = AsyncRuntime::new().map_err(|e| format!("could not create the JS runtime: {e}"))?;
    rt.set_memory_limit(shared.limits.memory_bytes).await;
    rt.set_max_stack_size(1 << 20).await;
    // Time spent waiting on the user doesn't count toward the timeout.
    let timeout = shared.limits.timeout;
    let host = shared.host.clone();
    let expired = move || Instant::now() >= started + timeout + host.user_wait();
    let flag = shared.cancelled.clone();
    let past = expired.clone();
    rt.set_interrupt_handler(Some(Box::new(move || flag.load(Ordering::Relaxed) || past())))
        .await;
    let ctx = AsyncContext::full(&rt)
        .await
        .map_err(|e| format!("could not create the JS context: {e}"))?;
    let flag = shared.cancelled.clone();
    let cancelled = async move {
        while !flag.load(Ordering::Relaxed) {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    };
    let timed_out = async move {
        while !expired() {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    };
    let code = body.clone();
    let script = ctx.async_with(async |ctx| -> Result<Option<Value>, String> {
        let bridge = bridge(&ctx, &shared).map_err(|e| format!("could not set up the script: {e}"))?;
        let prelude: Function = ctx.eval(PRELUDE).catch(&ctx).map_err(|e| format!("prelude: {e}"))?;
        prelude
            .call::<_, ()>((bridge,))
            .catch(&ctx)
            .map_err(|e| format!("prelude: {e}"))?;
        // Pre-run checks (L7.17) against what this engine really defines.
        let globals: Vec<String> = ctx
            // Global names include `Object.prototype`'s (`hasOwnProperty`, `toString`, …).
            .eval("[...Object.getOwnPropertyNames(globalThis), ...Object.getOwnPropertyNames(Object.prototype)]")
            .catch(&ctx)
            .map_err(|e| format!("prelude: {e}"))?;
        let (problems, decls) = crate::check::check(&body, &globals.into_iter().collect(), &shared.tools);
        if !problems.is_empty() {
            let mut e = format!("Script not run:\n{}", problems.join("\n"));
            if !decls.is_empty() {
                e.push_str(&format!("\n\n{}", decls.join("\n\n")));
            }
            return Err(e);
        }
        let mut opts = rquickjs::context::EvalOptions::default();
        opts.global = true;
        opts.strict = false;
        opts.filename = Some("script.js".into());
        let promise: Promise = ctx
            .eval_with_options(body, opts)
            .catch(&ctx)
            .map_err(|e| fix_lines(&e.to_string()))?;
        let value: Result<JsValue, _> = promise.into_future::<JsValue>().await.catch(&ctx);
        let value = match value {
            Ok(v) => v,
            Err(rquickjs::CaughtError::Value(v))
                if v.as_object().is_some_and(|o| o.get::<_, bool>("exit").unwrap_or(false)) =>
            {
                return Ok(None);
            }
            Err(e) => return Err(fix_lines(&e.to_string())),
        };
        if value.is_undefined() {
            return Ok(None);
        }
        let json = ctx
            .json_stringify(value)
            .catch(&ctx)
            .map_err(|e| format!("could not serialise the return value: {e}"))?;
        match json {
            Some(s) => {
                let s = s.to_string().map_err(|e| e.to_string())?;
                serde_json::from_str(&s).map(Some).map_err(|e| e.to_string())
            }
            None => Ok(None),
        }
    });
    let result = tokio::select! {
        r = script => r,
        _ = timed_out => Err(format!("timed out after {}s", timeout.as_secs())),
        _ = cancelled => Err("cancelled".into()),
    };
    let result = match result {
        Err(e) if e.contains("interrupted") && shared.cancelled.load(Ordering::Relaxed) => {
            Err(format!("cancelled ({e})"))
        }
        Err(e) if e.contains("interrupted") => Err(format!("timed out after {}s ({e})", timeout.as_secs())),
        Err(e) => match crate::check::hint(&e, &code) {
            Some(h) if !e.starts_with("Script not run:") => Err(format!("{e}\nHint: {h}")),
            _ => Err(e),
        },
        r => r,
    };
    drop(ctx);
    drop(rt);
    result
}

fn bridge<'js>(ctx: &rquickjs::Ctx<'js>, shared: &Arc<Shared>) -> rquickjs::Result<rquickjs::Object<'js>> {
    let b = rquickjs::Object::new(ctx.clone())?;
    let s = shared.clone();
    b.set(
        "out",
        Function::new(ctx.clone(), move |kind: String, text: String| {
            s.push(if kind == "text" {
                Item::Text(text)
            } else {
                Item::Console(text)
            })
        })?,
    )?;
    let s = shared.clone();
    b.set(
        "img",
        Function::new(ctx.clone(), move |data: String, mime: String| {
            s.push(Item::Image { data, mime })
        })?,
    )?;
    let s = shared.clone();
    b.set(
        "tools",
        Function::new(ctx.clone(), move || {
            Value::Array(
                s.tools
                    .iter()
                    .map(|t| json!({ "name": t.name, "description": t.description.lines().next().unwrap_or(""), "namespace": t.namespace }))
                    .collect(),
            )
            .to_string()
        })?,
    )?;
    let s = shared.clone();
    b.set(
        "search",
        Function::new(ctx.clone(), move |q: String, n: f64| {
            search(&s.tools, &q, n as usize).to_string()
        })?,
    )?;
    let s = shared.clone();
    b.set(
        "describe",
        Function::new(ctx.clone(), move |n: String| {
            let n = n.trim_start_matches("tools.").trim_start_matches("mcp.");
            let real = n.replacen('.', "__", 1);
            s.tools
                .iter()
                .find(|t| t.name == n || t.name == real)
                .map(declaration)
                .unwrap_or_default()
        })?,
    )?;
    let s = shared.clone();
    b.set(
        "namespace",
        Function::new(ctx.clone(), move |ns: String| {
            let ns = ns.trim_start_matches("mcp.");
            let d: Vec<String> = s
                .tools
                .iter()
                .filter(|t| t.namespace.as_deref() == Some(ns))
                .map(declaration)
                .collect();
            d.join("\n\n")
        })?,
    )?;
    let s = shared.clone();
    b.set(
        "set",
        Function::new(ctx.clone(), move |k: String, raw: String| {
            let v: Value = serde_json::from_str(&raw).unwrap_or(Value::Null);
            let mut store = s.store.lock().unwrap_or_else(|e| e.into_inner());
            store.set(k, v).err().unwrap_or_default()
        })?,
    )?;
    let s = shared.clone();
    b.set(
        "get",
        Function::new(ctx.clone(), move |k: String| {
            s.store
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .map
                .get(&k)
                .map(Value::to_string)
                .unwrap_or_default()
        })?,
    )?;
    let s = shared.clone();
    b.set(
        "call",
        Function::new(
            ctx.clone(),
            Async(move |name: String, raw: String| {
                let s = s.clone();
                async move { Ok::<String, rquickjs::Error>(call(&s, name, raw).await) }
            }),
        )?,
    )?;
    Ok(b)
}

async fn call(s: &Arc<Shared>, name: String, raw: String) -> String {
    let env = |r: Result<Value, String>| match r {
        Ok(v) => json!({ "ok": true, "value": v }).to_string(),
        Err(e) => json!({ "ok": false, "error": e }).to_string(),
    };
    if s.cancelled.load(Ordering::Relaxed) {
        return env(Err("cancelled".into()));
    }
    let Some(tool) = s.tools.iter().find(|t| t.name == name) else {
        return env(Err("unknown tool".into()));
    };
    if s.calls.fetch_add(1, Ordering::SeqCst) >= s.limits.max_calls {
        return env(Err(format!(
            "tool call limit reached ({} per script)",
            s.limits.max_calls
        )));
    }
    let args: Value = match serde_json::from_str(&raw) {
        Ok(Value::Null) => json!({}),
        Ok(v) => v,
        Err(e) => return env(Err(format!("arguments are not JSON: {e}"))),
    };
    // MCP servers get checked arguments; built-in tools parse their own (serde aliases).
    if tool.namespace.is_some()
        && let Err(e) = crate::check::validate(&tool.schema, &args)
    {
        return env(Err(format!("{e}\n{}", declaration(tool))));
    }
    let decl = tool.namespace.is_none().then(|| declaration(tool));
    struct AbortOnDrop(tokio::task::JoinHandle<Result<Value, String>>);
    impl Drop for AbortOnDrop {
        fn drop(&mut self) {
            self.0.abort();
        }
    }
    let mut h = AbortOnDrop(s.outer.spawn(s.host.call(name, args)));
    match (&mut h.0).await {
        Ok(Err(e)) if decl.is_some() && e.contains("invalid arguments") => {
            env(Err(format!("{e}\n{}", decl.unwrap_or_default())))
        }
        Ok(r) => env(r),
        Err(e) => env(Err(format!("tool task failed: {e}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Echo;
    impl Host for Echo {
        fn call(&self, name: String, args: Value) -> BoxFuture<'static, Result<Value, String>> {
            Box::pin(async move {
                tokio::time::sleep(Duration::from_millis(100)).await;
                if name == "fail" {
                    Err("boom".into())
                } else {
                    Ok(json!({ "tool": name, "args": args }))
                }
            })
        }
    }

    fn tools() -> Vec<ToolInfo> {
        let t = |n: &str, ns: Option<&str>| ToolInfo {
            name: n.into(),
            description: format!("{n} does things"),
            schema: json!({"type": "object", "properties": {"q": {"type": "string", "description": "query"}}, "required": ["q"]}),
            namespace: ns.map(String::from),
            output_schema: None,
        };
        vec![t("read", None), t("fail", None), t("ctx7__search", Some("ctx7"))]
    }

    async fn go(code: &str) -> Outcome {
        run(
            code,
            Arc::new(Echo),
            tools(),
            Map::new(),
            Limits::default(),
            Arc::new(AtomicBool::new(false)),
        )
        .await
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn models_classify_goes_through_the_classify_tool() {
        let mut list = tools();
        list.push(ToolInfo {
            name: "classify".into(),
            description: String::new(),
            schema: json!({}),
            namespace: None,
            output_schema: None,
        });
        let o = run(
            "const m = await models.getModelOfType('classifier', 'x', 'jev');
             const r = await models.classify(m, {state: {a: 1}, questions: {}});
             return [m.type, r.tool, r.args.model, r.args.state.a, (await models.getAvailableOfType('image')).length];",
            Arc::new(Fake),
            list,
            Map::new(),
            Limits::default(),
            Arc::new(AtomicBool::new(false)),
        )
        .await;
        assert_eq!(
            o.result,
            Some(json!(["classifier", "classify", "x/jev", 1, 0])),
            "{:?}",
            o.error
        );
        // Without the tool, there is no `models` global.
        assert_eq!(go("return typeof models").await.result, Some(json!("undefined")));
    }

    struct Fake;
    impl Host for Fake {
        fn call(&self, name: String, args: Value) -> BoxFuture<'static, Result<Value, String>> {
            Box::pin(async move {
                if args["list"] == true {
                    Ok(json!([{"provider": "x", "id": "jev"}]))
                } else {
                    Ok(json!({ "tool": name, "args": args }))
                }
            })
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn runs_calls_in_parallel_with_clean_output() {
        let t = Instant::now();
        let o = go("const [a, b, c] = await Promise.all([tools.read({q: 1}), mcp.ctx7.search({q: '2'}), tools.read({q: 3})]);\nconsole.log('n', 3);\ntext({ a: a.tool, b: b.tool });\nstore('k', c.args.q);\nreturn load('k');").await;
        assert_eq!(o.error, None);
        assert!(t.elapsed() < Duration::from_millis(280), "parallel: {:?}", t.elapsed());
        assert_eq!(o.calls, 3);
        assert_eq!(
            o.items,
            [
                Item::Console("n 3".into()),
                Item::Text(r#"{"a":"read","b":"ctx7__search"}"#.into())
            ]
        );
        assert_eq!(o.result, Some(json!(3)));
        assert_eq!(o.store["k"], json!(3));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn errors_name_the_line_and_limits_hold() {
        // A caught tool error names the model's line too.
        let o = go("let m;\ntry { await tools.fail({}); } catch (e) { m = e.message; }\nreturn m;").await;
        assert_eq!(o.result, Some(json!("fail: boom (at line 2)")));
        let o = go("const x = 1;\n\nawait tools.fail({});").await;
        let e = o.error.unwrap();
        assert!(e.contains("fail: boom") && e.contains("line 3"), "{e}");
        let o = go("const a = 1;\nnull.x;").await;
        assert!(o.error.unwrap().contains("line 2"));
        let o = go("text('a'); exit(); text('b');").await;
        assert_eq!((o.error, o.items), (None, vec![Item::Text("a".into())]));
        let o = run(
            "while (true) {}",
            Arc::new(Echo),
            vec![],
            Map::new(),
            Limits {
                timeout: Duration::from_millis(300),
                ..Limits::default()
            },
            Arc::new(AtomicBool::new(false)),
        )
        .await;
        assert!(o.error.unwrap().contains("timed out"));
        let o = go("const a = []; while (true) a.push('x'.repeat(1e6));").await;
        assert!(o.error.unwrap().contains("memory"), "64MB limit");
        let o = go("const r: number = (await tools.read({q: 'x'})).args.q.length; return r;").await;
        assert_eq!(o.result, Some(json!(1)), "TypeScript is accepted: {:?}", o.error);
        let o =
            go("return [(await searchTools('search'))[0].name, (await describeTool('mcp.ctx7.search')).includes('q: string')];").await;
        assert_eq!(o.result, Some(json!(["mcp.ctx7.search", true])));
        let o = go("return describeTool('read')").await;
        assert!(o.result.unwrap().as_str().unwrap().ends_with("): Promise<string>"));
        let mut t = tools().remove(0);
        t.output_schema = Some(json!({"type": "object", "properties": {"n": {"type": "integer"}}}));
        assert!(declaration(&t).ends_with("): Promise<{ n?: number }>"));
    }

    /// Records calls; `bash` fails like a non-zero exit, `read` without `path`
    /// fails like serde, everything else returns text.
    #[derive(Default)]
    struct Count(AtomicUsize);
    impl Host for Count {
        fn call(&self, name: String, args: Value) -> BoxFuture<'static, Result<Value, String>> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                match name.as_str() {
                    "bash" => Err("exit code 1".into()),
                    "read" if args.get("path").is_none() => Err("invalid arguments: missing field `path`".into()),
                    _ => Ok(json!("some text")),
                }
            })
        }
    }

    fn rich() -> Vec<ToolInfo> {
        let t = |n: &str, ns: Option<&str>, schema: Value| ToolInfo {
            name: n.into(),
            description: format!("{n} does things"),
            schema,
            namespace: ns.map(String::from),
            output_schema: None,
        };
        vec![
            t(
                "read",
                None,
                json!({"type": "object", "properties": {"path": {"type": "string"}, "offset": {"type": "integer"}}, "required": ["path"]}),
            ),
            t(
                "bash",
                None,
                json!({"type": "object", "properties": {"command": {"type": "string"}}, "required": ["command"]}),
            ),
            t(
                "ctx7__search",
                Some("ctx7"),
                json!({"type": "object", "properties": {
                    "query": {"type": "string"},
                    "limit": {"type": "integer"},
                    "mode": {"enum": ["a", "b"]},
                    "opts": {"type": "object", "properties": {"deep": {"type": "boolean"}}},
                    "tags": {"type": "array", "items": {"type": "string"}},
                    "any": {"anyOf": [{"type": "string"}]}
                }, "required": ["query"]}),
            ),
        ]
    }

    /// Runs `code` against [`rich`] tools; returns the outcome and the host calls made.
    async fn checked(code: &str) -> (Outcome, usize) {
        let host = Arc::new(Count::default());
        let o = run(
            code,
            host.clone(),
            rich(),
            Map::new(),
            Limits::default(),
            Arc::new(AtomicBool::new(false)),
        )
        .await;
        (o, host.0.load(Ordering::SeqCst))
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn missing_names_fail_before_any_call() {
        // The read on line 1 would run first; a failed check means no call at all.
        let (o, n) = checked("await tools.read({path: 'a'});\nconst e = require('fs');\nreturn proces.env;").await;
        let e = o.error.unwrap();
        assert_eq!(n, 0, "no host call: {e}");
        assert!(e.starts_with("Script not run:"), "{e}");
        assert!(
            e.contains("line 2: `require` doesn't exist here: scripts have no Node or browser APIs"),
            "{e}"
        );
        assert!(e.contains("line 3: `proces` is not defined"), "{e}");
        let (o, n) = checked("import fs from 'fs';\nreturn 1;").await;
        assert!(o.error.unwrap().contains("`import` doesn't exist here") && n == 0);
        let (o, _) = checked("const m = await import('fs');").await;
        assert!(o.error.unwrap().contains("`import()` doesn't exist here"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn valid_scripts_pass_the_checks() {
        let (o, n) = checked(
            "const env = typeof process === 'undefined' ? {} : process.env;\n\
             const local = 1;\ntotal = 2;\n\
             const ok = ['read' in tools, Object.keys(tools).length, typeof tools.call];\n\
             await tools.call({name: 'read', arguments: {path: 'a'}});\n\
             await tools.read({...{path: 'a'}});\nawait tools.read({path: 'a', file: 1});\n\
             return [local, total, ok];",
        )
        .await;
        assert_eq!(o.result, Some(json!([1, 2, [true, 2, "function"]])), "{:?}", o.error);
        assert_eq!(n, 3);
        // A script that writes globals skips the missing-name check.
        let (o, _) = checked("globalThis.made = 3;\nreturn made;").await;
        assert_eq!(o.result, Some(json!(3)), "{:?}", o.error);
        // Reads guarded by `try`, feature tests, `Object.prototype` names, `delete`.
        for (code, want) in [
            (
                "let e;\ntry { e = process.env } catch (x) { e = {} }\nreturn 1;",
                json!(1),
            ),
            (
                "if ('web_search' in tools) await tools.web_search({q: 1});\nreturn 1;",
                json!(1),
            ),
            ("if (mcp.ctx7.search_issues) return 1;\nreturn 2;", json!(2)),
            ("return mcp.ctx7.hasOwnProperty('search');", json!(true)),
            ("if ('gh' in mcp) await mcp.gh.search({});\nreturn 1;", json!(1)),
            (
                "if (Object.keys(tools).includes('web_search') && tools.web_search) await tools.web_search({});\nreturn 3;",
                json!(3),
            ),
            (
                "return [hasOwnProperty === Object.prototype.hasOwnProperty, toString.name, valueOf.name];",
                json!([true, "toString", "valueOf"]),
            ),
            ("return delete notdefined;", json!(true)),
        ] {
            let (o, _) = checked(code).await;
            assert_eq!(o.result, Some(want), "{code}: {:?}", o.error);
        }
        // Only the `try` block is guarded, not the handler.
        let (o, _) = checked("try { 1 } catch (x) { process.exit(1) }\nreturn 1;").await;
        assert!(o.error.unwrap().contains("`process` doesn't exist here"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn typescript_enums_run() {
        let o = go("enum Color { Red, Green }\nconst c: Color = Color.Green;\nreturn c;").await;
        assert_eq!(o.result, Some(json!(1)), "{:?}", o.error);
        let o = go("const enum E { A = 1 }\nreturn E.A;").await;
        assert_eq!(o.result, Some(json!(1)), "{:?}", o.error);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn unknown_tools_name_the_nearest() {
        let (o, n) = checked(
            "await tools.raed({path: 'a'});\nawait mcp.ctx8.search({query: 'q'});\n\
             await mcp.ctx7.serch({});\nawait callTool('bassh', {});\nawait tools['zzzzzzzz']();",
        )
        .await;
        let e = o.error.unwrap();
        assert_eq!(n, 0);
        for want in [
            "line 1: `tools.raed` is not a tool (nearest: tools.read)",
            "line 2: `mcp.ctx8` is not a MCP server (nearest: mcp.ctx7)",
            "line 3: `mcp.ctx7.serch` is not a tool (nearest: mcp.ctx7.search)",
            "line 4: `callTool(\"bassh\")` is not a tool (nearest: tools.bash)",
            "line 5: `tools.zzzzzzzz` is not a tool; find tools with searchTools",
        ] {
            assert!(e.contains(want), "{want}\n{e}");
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn literal_arguments_are_checked() {
        let (o, n) = checked("const a = 1;\nawait tools.read({file_path: 'x'});").await;
        let e = o.error.unwrap();
        assert_eq!(n, 0);
        assert!(
            e.contains("line 2: tools.read has no `file_path` (did you mean `path`?)"),
            "{e}"
        );
        assert!(
            e.contains("tools.read(args: { path: string; offset?: number }): Promise<string>"),
            "{e}"
        );
        let (o, _) = checked("await mcp.ctx7.search({limit: 3});").await;
        let e = o.error.unwrap();
        assert!(e.contains("line 1: mcp.ctx7.search is missing required `query`"), "{e}");
        assert!(e.contains("mcp.ctx7.search(args:"), "{e}");
        // A built-in missing a property with no near key is left to the tool.
        let (o, n) = checked("try { await tools.read({}) } catch (e) { return e.message }").await;
        assert_eq!(n, 1);
        assert!(
            o.result
                .unwrap()
                .as_str()
                .unwrap()
                .starts_with("read: invalid arguments")
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn mcp_arguments_are_validated_at_call_time() {
        let msg = async |args: &str| -> String {
            let (o, n) = checked(&format!(
                "const a = {args};\ntry {{ await mcp.ctx7.search(a) }} catch (e) {{ return e.message }}"
            ))
            .await;
            assert_eq!(n, 0, "{args}");
            o.result.and_then(|v| v.as_str().map(String::from)).unwrap_or_default()
        };
        let e = msg("{q: 'x'}").await;
        assert!(
            e.starts_with("mcp.ctx7.search: missing required \"query\"; \"q\" is not a parameter (did you mean \"query\"?) (at line 2)\n"),
            "{e}"
        );
        assert!(e.contains("\nmcp.ctx7.search(args: {"), "declaration: {e}");
        assert!(msg("{query: 1}").await.contains("\"query\" must be string"));
        assert!(
            msg("{query: 'x', mode: 'c'}")
                .await
                .contains("\"mode\" must be one of \"a\", \"b\"")
        );
        assert!(
            msg("{query: 'x', limit: 1.5}")
                .await
                .contains("\"limit\" must be integer")
        );
        assert!(
            msg("{query: 'x', opts: {deep: 'yes'}}")
                .await
                .contains("\"opts.deep\" must be boolean")
        );
        assert!(
            msg("{query: 'x', tags: ['a', 2]}")
                .await
                .contains("\"tags[1]\" must be string")
        );
        // Extras alone, numbers as integers, and anyOf parts pass.
        let (o, n) = checked("await mcp.ctx7.search({query: 'x', limit: 2, extra: 1, any: 5}); return 1").await;
        assert_eq!((o.error, n), (None, 1));
        // Lax inputs: numeric strings for numbers, null where `nullable: true`.
        let (o, n) = checked("await mcp.ctx7.search({query: 'x', limit: '5'}); return 1").await;
        assert_eq!((o.error, n), (None, 1));
        assert!(
            msg("{query: 'x', limit: '1.5'}")
                .await
                .contains("\"limit\" must be integer")
        );
        assert!(
            msg("{query: 'x', limit: 'five'}")
                .await
                .contains("\"limit\" must be integer")
        );
        let schema = json!({"type": "object", "properties": {
            "a": {"type": "string", "nullable": true}, "b": {"type": "string"}, "n": {"type": "number"}
        }});
        assert!(crate::check::validate(&schema, &json!({"a": null, "n": "2.5"})).is_ok());
        assert!(crate::check::validate(&schema, &json!({"b": null})).is_err());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn builtin_argument_errors_carry_the_declaration() {
        let (o, _) = checked("const a = {};\nawait tools.read(a);").await;
        let e = o.error.unwrap();
        assert!(
            e.contains(
                "read: invalid arguments: missing field `path` (at line 2)\n/** read does things */\ntools.read(args:"
            ),
            "{e}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn store_is_capped() {
        let o = go(
            "store('a', 'x'.repeat(5 << 20));\nlet m;\ntry { store('b', 'x'.repeat(4 << 20)); } catch (e) { m = e.message; }\n\
             store('a', 'y'.repeat(5 << 20));\nreturn [m, load('b') === undefined, load('a')[0]];",
        )
        .await;
        let r = o.result.unwrap();
        assert!(r[0].as_str().unwrap().starts_with("store is full (8 MiB)"), "{r}");
        assert_eq!((&r[1], &r[2]), (&json!(true), &json!("y")));
        assert_eq!(o.store.len(), 1);
        // Overwriting with a smaller value frees space; the cap still holds after.
        let o = go(
            "store('a', 'x'.repeat(5 << 20));\nstore('a', 1);\nstore('b', 'x'.repeat(7 << 20));\n\
             try { store('c', 'x'.repeat(1 << 20)); return 'stored' } catch (e) { return e.message }",
        )
        .await;
        assert!(o.result.unwrap().as_str().unwrap().starts_with("store is full"));
        // The incoming store counts toward the cap.
        let mut full = Map::new();
        full.insert("old".into(), json!("x".repeat(8 << 20)));
        let o = run(
            "try { store('n', 'abc'); return 'stored' } catch (e) { return e.message }",
            Arc::new(Echo),
            tools(),
            full,
            Limits::default(),
            Arc::new(AtomicBool::new(false)),
        )
        .await;
        assert!(o.result.unwrap().as_str().unwrap().starts_with("store is full"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn failed_scripts_get_one_hint() {
        let hint = async |code: &str| -> String {
            let e = checked(code).await.0.error.unwrap();
            assert_eq!(e.matches("Hint: ").count(), 1, "{e}");
            e
        };
        // `eval` turns the missing-name check off, so this fails at run time.
        assert!(
            hint("eval('1');\nprocess.exit(1);")
                .await
                .contains("Hint: scripts have no Node or browser APIs")
        );
        assert!(
            hint("(await tools.read({path: 'a'})).map((x) => x);")
                .await
                .contains("Hint: tools.* return text")
        );
        assert!(
            hint("const r = undefined;\nr.x;")
                .await
                .contains("Hint: tools.* return text")
        );
        assert!(
            hint("JSON.parse(await tools.read({path: 'a'}));")
                .await
                .contains("Hint: tool output is plain text")
        );
        assert!(
            hint("await tools.bash({command: 'false'});")
                .await
                .contains("Hint: tools.bash throws on a non-zero exit")
        );
        assert!(
            hint("const s = `a `b` c`;")
                .await
                .contains("Hint: a backtick inside a template string")
        );
        // No hint for other errors.
        assert!(!checked("throw new Error('x')").await.0.error.unwrap().contains("Hint"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn structured_clone_is_a_json_copy() {
        let o = go("const a = {x: [1]};\nconst b = structuredClone(a);\nb.x.push(2);\nreturn [a.x.length, b.x.length, structuredClone(undefined) === undefined];").await;
        assert_eq!(o.result, Some(json!([1, 2, true])), "{:?}", o.error);
    }

    /// A call that waits `wait` on the user; `counts` = whether it reports that wait.
    struct Waits {
        wait: Duration,
        counts: bool,
        since: Mutex<Option<Instant>>,
    }
    impl Host for Waits {
        fn call(&self, _: String, _: Value) -> BoxFuture<'static, Result<Value, String>> {
            *self.since.lock().unwrap() = Some(Instant::now());
            let wait = self.wait;
            Box::pin(async move {
                tokio::time::sleep(wait).await;
                Ok(json!("ok"))
            })
        }
        fn user_wait(&self) -> Duration {
            match *self.since.lock().unwrap() {
                Some(t) if self.counts => t.elapsed().min(self.wait),
                _ => Duration::ZERO,
            }
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn user_wait_does_not_count_toward_the_timeout() {
        for counts in [true, false] {
            let host = Arc::new(Waits {
                wait: Duration::from_millis(700),
                counts,
                since: Mutex::new(None),
            });
            let o = run(
                "await tools.read({q: 'a'});\nreturn 1;",
                host,
                tools(),
                Map::new(),
                Limits {
                    timeout: Duration::from_millis(300),
                    ..Limits::default()
                },
                Arc::new(AtomicBool::new(false)),
            )
            .await;
            if counts {
                assert_eq!((o.result, o.error), (Some(json!(1)), None));
            } else {
                assert!(o.error.unwrap().contains("timed out"));
            }
        }
    }

    /// Context creation + TS strip cost (ARCHITECTURE §6 "to verify").
    #[tokio::test(flavor = "multi_thread")]
    async fn startup_is_cheap() {
        go("return 1").await;
        let t = Instant::now();
        for _ in 0..20 {
            go("return 1").await;
        }
        let per = t.elapsed() / 20;
        let t = Instant::now();
        crate::ts::to_js("const a: number = 1; interface X { y: string }\nreturn a;", false);
        eprintln!("codemode run: {per:?} per script; ts strip {:?}", t.elapsed());
        assert!(per < Duration::from_millis(50));
    }
}
