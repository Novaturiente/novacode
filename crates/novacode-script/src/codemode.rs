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
}

#[derive(Clone, Debug)]
pub struct ToolInfo {
    pub name: String,
    pub description: String,
    pub schema: Value,
    /// MCP server for `server__tool` names; reached as `mcp.<server>.<tool>()`.
    pub namespace: Option<String>,
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
  const call = async (name, args) => {
    // Async frames are lost after the await, so note the script's line now.
    const site = (/script\.js:\d+/.exec(new Error().stack || "") || [""])[0];
    const r = parse(await bridge.call(String(name), stringify(args === undefined ? {} : args)));
    if (!r.ok) { const e = new Error(`${name}: ${r.error}${site ? ` (at ${site})` : ""}`); e.tool = name; throw e; }
    return r.value;
  };
  globalThis.callTool = call;
  const list = parse(bridge.tools());
  const names = new Set(list.map((t) => t.name));
  const unknown = (name) => { throw new Error(`unknown tool '${name}'. Use searchTools("…") to find tools.`); };
  globalThis.tools = new Proxy({}, {
    get(_, n) { if (typeof n !== "string" || n === "then") return undefined; return names.has(n) ? (a) => call(n, a) : (a) => Promise.reject(new Error(`unknown tool '${n}'. Use searchTools("…") to find tools.`)); },
    has(_, n) { return names.has(String(n)); },
    ownKeys() { return list.filter((t) => !t.namespace).map((t) => t.name); },
    getOwnPropertyDescriptor(_, n) { return names.has(String(n)) ? { enumerable: true, configurable: true } : undefined; },
  });
  const spaces = {};
  for (const t of list) if (t.namespace) {
    const short = t.name.slice(t.namespace.length + 2);
    (spaces[t.namespace] ||= {})[short] = (a) => call(t.name, a);
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
  globalThis.store = (k, v) => { bridge.set(String(k), stringify(v === undefined ? null : v)); return v; };
  globalThis.load = (k) => { const r = bridge.get(String(k)); return r === "" ? undefined : parse(r); };
})
"#;

struct Shared {
    host: Arc<dyn Host>,
    outer: tokio::runtime::Handle,
    tools: Vec<ToolInfo>,
    store: Mutex<Map<String, Value>>,
    items: Mutex<(Vec<Item>, usize, bool)>,
    calls: AtomicUsize,
    cancelled: Arc<AtomicBool>,
    limits: Limits,
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
fn js_name(t: &ToolInfo) -> String {
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
    format!("/** {first} */\n{}(args: {}): Promise<any>", js_name(t), obj(&t.schema))
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
        store: Mutex::new(store),
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
    let store = std::mem::take(&mut *shared.store.lock().unwrap_or_else(|e| e.into_inner()));
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
    let deadline = started + shared.limits.timeout;
    let flag = shared.cancelled.clone();
    rt.set_interrupt_handler(Some(Box::new(move || {
        flag.load(Ordering::Relaxed) || Instant::now() >= deadline
    })))
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
    let timeout = shared.limits.timeout;
    let script = ctx.async_with(async |ctx| -> Result<Option<Value>, String> {
        let bridge = bridge(&ctx, &shared).map_err(|e| format!("could not set up the script: {e}"))?;
        let prelude: Function = ctx.eval(PRELUDE).catch(&ctx).map_err(|e| format!("prelude: {e}"))?;
        prelude
            .call::<_, ()>((bridge,))
            .catch(&ctx)
            .map_err(|e| format!("prelude: {e}"))?;
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
        _ = tokio::time::sleep(timeout) => Err(format!("timed out after {}s", timeout.as_secs())),
        _ = cancelled => Err("cancelled".into()),
    };
    let result = match result {
        Err(e) if e.contains("interrupted") && shared.cancelled.load(Ordering::Relaxed) => {
            Err(format!("cancelled ({e})"))
        }
        Err(e) if e.contains("interrupted") => Err(format!("timed out after {}s ({e})", timeout.as_secs())),
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
            s.store
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(k, serde_json::from_str(&raw).unwrap_or(Value::Null));
        })?,
    )?;
    let s = shared.clone();
    b.set(
        "get",
        Function::new(ctx.clone(), move |k: String| {
            s.store
                .lock()
                .unwrap_or_else(|e| e.into_inner())
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
    if !s.tools.iter().any(|t| t.name == name) {
        return env(Err("unknown tool".into()));
    }
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
    struct AbortOnDrop(tokio::task::JoinHandle<Result<Value, String>>);
    impl Drop for AbortOnDrop {
        fn drop(&mut self) {
            self.0.abort();
        }
    }
    let mut h = AbortOnDrop(s.outer.spawn(s.host.call(name, args)));
    match (&mut h.0).await {
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
        let o = go("const [a, b, c] = await Promise.all([tools.read({q: 1}), mcp.ctx7.search({q: 2}), tools.read({q: 3})]);\nconsole.log('n', 3);\ntext({ a: a.tool, b: b.tool });\nstore('k', c.args.q);\nreturn load('k');").await;
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
