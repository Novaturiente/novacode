//! The `codemode` tool (L7.5): the model writes a short JS/TS script that chains
//! tool and MCP calls. Every call goes through the session's pipeline ([`Runner`]),
//! so permissions, hooks and output guards apply; only what the script prints
//! (`text`, `console`, `image`, `return`) reaches the model.

use crate::pipeline::{self, Prepared};
use crate::policy::{Approver, Policy};
use futures_util::future::BoxFuture;
use novacode_base::redact::Redactor;
use novacode_script::codemode::{self as engine, Host, Item, Limits, ToolInfo};
use novacode_tools::guard::Budget;
use novacode_tools::{Nested, Tool, ToolCtx, ToolOutput};
use novacode_types::{Block, ToolCall, ToolDef};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Map, Value, json};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

pub const NAME: &str = "codemode";

/// Runs nested calls through the same pipeline as the model's own (L3.11).
pub struct Runner {
    pub tools: Vec<Arc<dyn Tool>>,
    pub policy: Arc<Policy>,
    pub approver: Option<Arc<dyn Approver>>,
    pub spill_dir: PathBuf,
    pub redactor: Redactor,
    pub budget: Budget,
    pub seq: AtomicUsize,
    pub hooks: Arc<crate::hooks::Hooks>,
}

impl Nested for Runner {
    fn call(&self, name: String, args: Value, ctx: ToolCtx) -> BoxFuture<'static, ToolOutput> {
        let n = self.seq.fetch_add(1, Ordering::Relaxed);
        let call = ToolCall {
            id: format!("script-{n}"),
            name,
            arguments: args,
            arguments_error: None,
            replay: None,
        };
        let prepared = pipeline::prepare(&self.tools, &call);
        let (policy, approver, redactor, budget, hooks) = (
            self.policy.clone(),
            self.approver.clone(),
            self.redactor.clone(),
            self.budget,
            self.hooks.clone(),
        );
        let spill = self.spill_dir.join(format!("{}-script-{n}.txt", ctx.session_id));
        Box::pin(async move {
            let (tool, args, accept) = match prepared {
                Prepared::Rejected(o) => return o,
                Prepared::Ready(t, a, acc) => (t, a, acc),
            };
            let args = match hooks.before_tool(&call.name, args).await {
                Ok(a) => a,
                Err(out) => return out,
            };
            if let Some(refusal) = pipeline::authorize(
                &policy,
                approver.as_ref(),
                &call.name,
                tool.read_only(),
                &args,
                &ctx.cwd,
            )
            .await
            {
                return refusal;
            }
            let started = hooks.start(&call.name, &ctx.cwd).await;
            let mut out = pipeline::run(tool.as_ref(), args.clone(), accept, &ctx, &spill, budget, &redactor).await;
            hooks
                .after_tool(&call.id, &call.name, &args, &ctx.cwd, started, &mut out)
                .await;
            out
        })
    }

    fn tools(&self) -> Vec<(ToolDef, Option<String>)> {
        self.tools
            .iter()
            .map(|t| {
                let ns = t.name().split_once("__").map(|(s, _)| s.to_string());
                (pipeline::def(t.as_ref()), ns)
            })
            .collect()
    }
}

/// The tool; `store`/`load` values live for the session (shortcut: lost on restart).
pub struct Codemode {
    pub store: Mutex<Map<String, Value>>,
    description: String,
}

const DESCRIPTION: &str = "Run a short JavaScript/TypeScript script that calls tools, to chain several calls (or MCP calls) and return only the result you need. \
`await tools.<name>(args)` calls any tool, deferred ones too (resolves to its full text output: never clipped or paged, so `JSON.parse(await tools.read({path}))` works; throws on error); `await mcp.<server>.<tool>(args)` calls an MCP tool (`mcp.server[\"tool-name\"]` for dashed names). \
Use `Promise.all` for parallel calls. Only `text(v)`, `console.log(...)`, `image(dataUrl)` and the `return` value reach you. \
`await searchTools(query)`, `await describeTool(name)` and `await describeNamespace(server)` show what exists and its argument types. `store(key, v)`/`load(key)` keep JSON across scripts. \
Classifier models answer typed questions about JSON with probabilities: `await models.getAvailableOfType(\"classifier\")`, then \
`await models.classify(model, {state, questions: {id: {type: \"choice\"|\"score\"|\"bool\", instructions, criteria}}})` (check `stopReason`).";

impl Codemode {
    /// `servers`: MCP (name, description), fixed for the session so the tool list stays cacheable.
    pub fn new(servers: &[(String, String)]) -> Self {
        let mut description = DESCRIPTION.to_string();
        if !servers.is_empty() {
            description.push_str("\nMCP servers (describeNamespace for their tools):");
            for (n, d) in servers {
                description.push_str(&format!(
                    "\n- {n}{}",
                    if d.is_empty() { String::new() } else { format!(": {d}") }
                ));
            }
        }
        Codemode {
            store: Mutex::default(),
            description,
        }
    }
}

impl Default for Codemode {
    fn default() -> Self {
        Self::new(&[])
    }
}

#[derive(Deserialize, JsonSchema)]
struct Args {
    /// JavaScript or TypeScript: the body of an async function (top-level await and return work).
    code: String,
    /// Undo the script's file changes (write/edit/apply_patch) if it throws. Bash and MCP effects can't be undone.
    #[serde(default)]
    transaction: bool,
}

/// Files a call is about to change, from its arguments.
fn targets(name: &str, args: &Value, ctx: &ToolCtx) -> Vec<PathBuf> {
    match name {
        "write" | "edit" => args["path"].as_str().map(|p| vec![ctx.path(p)]).unwrap_or_default(),
        "apply_patch" => args["patch"]
            .as_str()
            .or(args["input"].as_str())
            .unwrap_or_default()
            .lines()
            .filter_map(|l| {
                [
                    "*** Update File: ",
                    "*** Add File: ",
                    "*** Delete File: ",
                    "*** Move to: ",
                ]
                .iter()
                .find_map(|p| l.strip_prefix(p))
            })
            .map(|p| ctx.path(p.trim()))
            .collect(),
        _ => vec![],
    }
}

struct ScriptHost {
    nested: Arc<dyn Nested>,
    ctx: ToolCtx,
    updates: Option<tokio::sync::mpsc::Sender<String>>,
    /// Transaction mode: each touched file's content before the script (None = didn't exist).
    undo: Option<Undo>,
}

impl Host for ScriptHost {
    fn call(&self, name: String, args: Value) -> BoxFuture<'static, Result<Value, String>> {
        if let Some(undo) = &self.undo {
            let mut u = undo.lock().unwrap();
            for p in targets(&name, &args, &self.ctx) {
                u.entry(p.clone()).or_insert_with(|| std::fs::read(&p).ok());
            }
        }
        let short = {
            let a = args.to_string();
            let cut: String = a.chars().take(80).collect();
            if cut.len() < a.len() { format!("{cut}…") } else { cut }
        };
        let fut = self.nested.call(name.clone(), args, self.ctx.clone());
        let updates = self.updates.clone();
        Box::pin(async move {
            let t0 = Instant::now();
            let out = fut.await;
            if let Some(u) = updates {
                let mark = if out.is_error { "✗" } else { "✓" };
                let _ = u.try_send(format!("{mark} {name}({short}) {}ms\n", t0.elapsed().as_millis()));
            }
            let text: String = out
                .content
                .iter()
                .filter_map(|b| match b {
                    Block::Text { text, .. } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n");
            if out.is_error {
                return Err(text);
            }
            // MCP tools hand back structured content when they have it.
            Ok(out
                .details
                .as_ref()
                .and_then(|d| d.get("structured"))
                .cloned()
                .unwrap_or(Value::String(text)))
        })
    }
}

fn rollback(undo: &HashMap<PathBuf, Option<Vec<u8>>>) -> usize {
    for (p, before) in undo {
        let _ = match before {
            Some(b) => std::fs::write(p, b),
            None => std::fs::remove_file(p),
        };
    }
    undo.len()
}

impl Tool for Codemode {
    fn name(&self) -> &str {
        NAME
    }
    fn description(&self) -> &str {
        &self.description
    }
    fn schema(&self) -> Value {
        novacode_tools::schema_for::<Args>()
    }
    fn guidelines(&self) -> &[&str] {
        &[
            "Use codemode to chain 3+ dependent tool or MCP calls, or to filter large output down to what you need; use plain tool calls for single steps.",
        ]
    }
    fn execute<'a>(&'a self, args: Value, ctx: &'a ToolCtx) -> BoxFuture<'a, ToolOutput> {
        Box::pin(async move {
            let a: Args = match novacode_tools::parse(args) {
                Ok(a) => a,
                Err(e) => return e,
            };
            let Some(nested) = ctx.nested.clone() else {
                return ToolOutput::error("codemode can't run here (nested scripts are not allowed)");
            };
            let tools: Vec<ToolInfo> = nested
                .tools()
                .into_iter()
                .map(|(d, ns)| ToolInfo {
                    name: d.name,
                    description: d.description,
                    schema: d.schema,
                    namespace: ns,
                })
                .collect();
            let mut inner = ctx.clone();
            inner.nested = None;
            inner.updates = None;
            inner.script = true;
            let undo = a.transaction.then(Arc::default);
            let host = Arc::new(ScriptHost {
                nested,
                ctx: inner,
                updates: ctx.updates.clone(),
                undo: undo.clone(),
            });
            let cancel = Arc::new(AtomicBool::new(false));
            let (flag, token) = (cancel.clone(), ctx.cancel.clone());
            let watch = tokio::spawn(async move {
                token.cancelled().await;
                flag.store(true, Ordering::SeqCst);
            });
            let store = self.store.lock().unwrap().clone();
            let o = engine::run(&a.code, host, tools, store, Limits::default(), cancel).await;
            watch.abort();
            *self.store.lock().unwrap() = o.store.clone();
            render(o, undo)
        })
    }
}

type Undo = Arc<Mutex<HashMap<PathBuf, Option<Vec<u8>>>>>;

fn render(o: engine::Outcome, undo: Option<Undo>) -> ToolOutput {
    let mut content = Vec::new();
    let mut text = String::new();
    for item in o.items {
        match item {
            Item::Text(t) | Item::Console(t) => {
                text.push_str(&t);
                text.push('\n');
            }
            Item::Image { data, mime } => {
                if !text.is_empty() {
                    content.push(Block::text(std::mem::take(&mut text).trim_end().to_string()));
                }
                content.push(Block::Image { data, mime_type: mime });
            }
        }
    }
    if let Some(v) = &o.result {
        match v {
            Value::String(s) => text.push_str(s),
            v => text.push_str(&serde_json::to_string_pretty(v).unwrap_or_default()),
        }
    }
    if o.truncated {
        text.push_str("\n[output cut: the script printed more than 16 MB]");
    }
    let is_error = o.error.is_some();
    if let Some(e) = &o.error {
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str(&format!("Script failed: {e}"));
        if let Some(u) = &undo {
            let n = rollback(&u.lock().unwrap());
            if n > 0 {
                text.push_str(&format!("\n(transaction rolled back: {n} file(s) restored)"));
            }
        }
    }
    if !text.trim().is_empty() || content.is_empty() {
        let t = text.trim_end();
        content.push(Block::text(if t.is_empty() { "(no output)" } else { t }.to_string()));
    }
    ToolOutput {
        content,
        is_error,
        details: Some(json!({ "calls": o.calls, "elapsed_ms": o.elapsed.as_millis() as u64 })),
    }
}
