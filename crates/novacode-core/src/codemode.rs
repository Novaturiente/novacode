//! The `codemode` tool (L7.5): the model writes a short JS/TS script that chains
//! tool and MCP calls. Every call goes through the session's pipeline ([`Runner`]),
//! so permissions, hooks and output guards apply; only what the script prints
//! (`text`, `console`, `image`, `return`) reaches the model.

use crate::pipeline::{self, Prepared};
use crate::policy::{Approval, ApprovalRequest, Approver, Policy};
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
use std::time::{Duration, Instant};

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
    /// The command judge (L6.8), `None` = off.
    pub judge: Option<Arc<crate::judge::Judge>>,
    /// Time nested calls spent waiting on the user; it doesn't count toward a script's timeout.
    pub wait: Arc<WaitClock>,
}

/// Wall time during which at least one wait on the user was going.
#[derive(Default)]
pub struct WaitClock(Mutex<WaitState>);

#[derive(Default)]
struct WaitState {
    total: Duration,
    open: usize,
    since: Option<Instant>,
}

impl WaitClock {
    /// The clock now, a wait still going counted up to now.
    pub fn now(&self) -> Duration {
        let s = self.0.lock().unwrap();
        s.total + s.since.map(|t| t.elapsed()).unwrap_or_default()
    }

    /// Start a wait; it ends when the guard drops (also on cancel).
    pub fn begin(self: &Arc<Self>) -> WaitGuard {
        let mut s = self.0.lock().unwrap();
        if s.open == 0 {
            s.since = Some(Instant::now());
        }
        s.open += 1;
        WaitGuard(self.clone())
    }
}

pub struct WaitGuard(Arc<WaitClock>);

impl Drop for WaitGuard {
    fn drop(&mut self) {
        let mut s = self.0.0.lock().unwrap();
        s.open -= 1;
        if s.open == 0
            && let Some(t) = s.since.take()
        {
            s.total += t.elapsed();
        }
    }
}

/// Times the approval cards of nested calls.
struct TimedApprover {
    inner: Arc<dyn Approver>,
    clock: Arc<WaitClock>,
}

impl Approver for TimedApprover {
    fn approve(&self, req: ApprovalRequest) -> BoxFuture<'_, Approval> {
        Box::pin(async move {
            let _wait = self.clock.begin();
            self.inner.approve(req).await
        })
    }
}

/// Marks a refused call so the script host stops the script's tool use.
fn refused(mut out: ToolOutput) -> ToolOutput {
    out.details = Some(match out.details.take() {
        Some(Value::Object(mut m)) => {
            m.insert("refused".into(), Value::Bool(true));
            Value::Object(m)
        }
        None => json!({ "refused": true }),
        Some(other) => json!({ "refused": true, "details": other }),
    });
    out
}

impl Nested for Runner {
    fn user_wait(&self) -> Duration {
        self.wait.now()
    }

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
        let wait = self.wait.clone();
        let approver: Option<Arc<dyn Approver>> = self.approver.clone().map(|inner| {
            Arc::new(TimedApprover {
                inner,
                clock: wait.clone(),
            }) as Arc<dyn Approver>
        });
        let (policy, redactor, budget, hooks, judge) = (
            self.policy.clone(),
            self.redactor.clone(),
            self.budget,
            self.hooks.clone(),
            self.judge.clone(),
        );
        let spill = self.spill_dir.join(format!("{}-script-{n}.txt", ctx.session_id));
        Box::pin(async move {
            // The whole of an `ask_user` call is time waiting on the user.
            let _wait = (call.name == "ask_user").then(|| wait.begin());
            let (tool, args, accept) = match prepared {
                Prepared::Rejected(o) => return o,
                Prepared::Ready(t, a, acc) => (t, a, acc),
            };
            let (args, decision) = match hooks.before_tool(&call.name, args).await {
                Ok(a) => a,
                Err(out) => return refused(out),
            };
            if let Some(refusal) = pipeline::authorize_cancellable(
                &policy,
                approver.as_ref(),
                judge.as_deref(),
                &call.name,
                tool.read_only(),
                &args,
                &ctx.cwd,
                &decision,
                &ctx.cancel,
            )
            .await
            {
                return refused(refusal);
            }
            // The script's token: cancelled by its first refusal, so a call allowed
            // just as that lands still doesn't run.
            if ctx.cancel.is_cancelled() {
                return refused(ToolOutput::error(
                    "Not run: this script stopped calling tools after a refused call.",
                ));
            }
            let started = hooks.start(&call.name, &ctx.cwd).await;
            let mut out = pipeline::run(tool.as_ref(), args.clone(), accept, &ctx, &spill, budget, &redactor).await;
            hooks
                .after_tool(&call.id, &call.name, &args, &ctx.cwd, started, &mut out)
                .await;
            out
        })
    }

    fn tools(&self) -> Vec<(ToolDef, Option<String>, Option<Value>)> {
        self.tools
            .iter()
            .map(|t| {
                let ns = t.name().split_once("__").map(|(s, _)| s.to_string());
                (pipeline::def(t.as_ref()), ns, t.output_schema())
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
`await tools.<name>(args)` calls any tool, deferred ones too (resolves to its full text output: never clipped or paged, so `JSON.parse(await tools.read({path}))` works; throws on error, and `tools.bash` throws on a non-zero exit with the output in the error); `await mcp.<server>.<tool>(args)` calls an MCP tool (`mcp.server[\"tool-name\"]` for dashed names) and resolves to an object when the tool has an output schema. A result with images resolves to `{text, images}`. \
An uncaught error stops the script; the reply then lists the calls that ran. There are no Node or browser APIs (no require/import, fs, process, fetch, timers): everything goes through tools. \
Use `Promise.all` for parallel calls. Only `text(v)`, `console.log(...)`, `image(dataUrl or an item of images)` and the `return` value reach you. \
`await searchTools(query)`, `await describeTool(name)` and `await describeNamespace(server)` show what exists, its argument types and return type. `store(key, v)`/`load(key)` keep JSON across scripts. \
Classifier models answer typed questions about JSON with probabilities: `await models.getAvailableOfType(\"classifier\")`, then \
`await models.classify(model, {state, questions: {id: {type: \"choice\"|\"score\"|\"bool\", instructions, criteria}}})` (check `stopReason`).";

impl Codemode {
    /// `servers`: MCP (name, description), fixed for the session so the tool list stays cacheable.
    pub fn new(servers: &[(String, String)]) -> Self {
        Self::with_signatures(servers, &[])
    }

    /// Like [`Codemode::new`], plus MCP tools known from an earlier connect, whose
    /// signatures go into the description within a budget (L7.5).
    pub fn with_signatures(servers: &[(String, String)], tools: &[ToolInfo]) -> Self {
        let mut description = DESCRIPTION.to_string();
        if !servers.is_empty() {
            description.push_str("\nMCP servers (describeNamespace for their tools):");
            let mut used = 0;
            for (n, d) in servers {
                description.push_str(&format!(
                    "\n- {n}{}",
                    if d.is_empty() { String::new() } else { format!(": {d}") }
                ));
                let mine: Vec<&ToolInfo> = tools.iter().filter(|t| t.namespace.as_deref() == Some(n)).collect();
                if mine.is_empty() {
                    continue;
                }
                let full: String = mine.iter().map(|t| format!("\n  {}", signature(t))).collect();
                let names = format!(
                    "\n  tools: {}",
                    mine.iter()
                        .map(|t| &t.name[n.len() + 2..])
                        .collect::<Vec<_>>()
                        .join(", ")
                );
                for block in [full, names] {
                    let len = block.chars().count();
                    if used + len <= SIGNATURE_BUDGET {
                        used += len;
                        description.push_str(&block);
                        break;
                    }
                }
            }
        }
        Codemode {
            store: Mutex::default(),
            description,
        }
    }
}

/// Characters of MCP signature lines in the description, all servers together.
const SIGNATURE_BUDGET: usize = 6_000;

/// One compact line: the declaration's signature and the description's first sentence.
fn signature(t: &ToolInfo) -> String {
    let decl = engine::declaration(t);
    let sig = decl.lines().nth(1).unwrap_or_default();
    let first = t.description.lines().next().unwrap_or_default();
    let sentence = first.split_once(". ").map(|(s, _)| s).unwrap_or(first);
    let cut: String = sentence.chars().take(80).collect();
    if cut.is_empty() {
        sig.to_string()
    } else if cut.len() < sentence.len() {
        format!("{sig} — {cut}…")
    } else {
        format!("{sig} — {cut}")
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
    /// Its `cancel` is the script's own child token: the first refusal cancels it,
    /// which ends the approval cards still open and cancels the calls still running.
    /// The script itself keeps running (the engine follows only the outer token);
    /// its later calls fail with the stop message.
    ctx: ToolCtx,
    updates: Option<tokio::sync::mpsc::Sender<String>>,
    /// Transaction mode: each touched file's content before the script (None = didn't exist).
    undo: Option<Undo>,
    log: Arc<Mutex<Log>>,
    /// The nested runner's user-wait clock when the script started.
    wait0: Duration,
}

/// What the script's calls did, for the reply.
#[derive(Default)]
struct Log {
    /// First line of the first refused call; later calls don't run.
    refused: Option<String>,
    /// Finished calls, as sent to `updates`.
    ran: Vec<String>,
}

impl Host for ScriptHost {
    // shortcut: the clock is the runner's, so two scripts running at the same time
    // both get the other's waits too.
    fn user_wait(&self) -> Duration {
        self.nested.user_wait().saturating_sub(self.wait0)
    }

    fn call(&self, name: String, args: Value) -> BoxFuture<'static, Result<Value, String>> {
        if let Some(r) = self.log.lock().unwrap().refused.clone() {
            return Box::pin(async move {
                Err(format!(
                    "not run: this script stopped calling tools after a refused call ({r})"
                ))
            });
        }
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
        let log = self.log.clone();
        let stop = self.ctx.cancel.clone();
        Box::pin(async move {
            let t0 = Instant::now();
            let out = fut.await;
            let mark = if out.is_error { "✗" } else { "✓" };
            let line = format!("{mark} {name}({short}) {}ms", t0.elapsed().as_millis());
            if let Some(u) = updates {
                let _ = u.try_send(format!("{line}\n"));
            }
            let refusal = out.details.as_ref().is_some_and(|d| d["refused"] == true);
            {
                let mut l = log.lock().unwrap();
                l.ran.push(line);
                if refusal && l.refused.is_none() {
                    l.refused = Some(first_line(&out));
                    stop.cancel();
                }
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
            let images: Vec<Value> = out
                .content
                .iter()
                .filter_map(|b| match b {
                    Block::Image { data, mime_type } => {
                        Some(json!({ "type": "image", "data": data, "mimeType": mime_type }))
                    }
                    _ => None,
                })
                .collect();
            Ok(script_value(
                text,
                images,
                out.details.as_ref().and_then(|d| d.get("structured")),
            ))
        })
    }
}

/// What a nested call resolves to in the script. MCP tools hand back structured
/// content when they have it; images ride along with it or with the text.
fn script_value(text: String, images: Vec<Value>, structured: Option<&Value>) -> Value {
    match (structured, images.is_empty()) {
        (Some(s), true) => s.clone(),
        (Some(Value::Object(m)), false) if !m.contains_key("images") => {
            let mut m = m.clone();
            m.insert("images".into(), Value::Array(images));
            Value::Object(m)
        }
        (Some(s), false) => json!({ "structured": s, "images": images }),
        (None, true) => Value::String(text),
        (None, false) => json!({ "text": text, "images": images }),
    }
}

/// The first non-empty text line of an output.
fn first_line(out: &ToolOutput) -> String {
    out.content
        .iter()
        .filter_map(|b| match b {
            Block::Text { text, .. } => text.lines().find(|l| !l.trim().is_empty()),
            _ => None,
        })
        .next()
        .unwrap_or("refused")
        .trim()
        .to_string()
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
                .map(|(d, ns, output_schema)| ToolInfo {
                    name: d.name,
                    description: d.description,
                    schema: d.schema,
                    namespace: ns,
                    output_schema,
                })
                .collect();
            let mut inner = ctx.clone();
            inner.cancel = ctx.cancel.child_token();
            inner.nested = None;
            inner.updates = None;
            inner.script = true;
            let undo = a.transaction.then(Arc::default);
            let log = Arc::new(Mutex::new(Log::default()));
            let host = Arc::new(ScriptHost {
                wait0: nested.user_wait(),
                nested,
                ctx: inner,
                updates: ctx.updates.clone(),
                undo: undo.clone(),
                log: log.clone(),
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
            render(o, undo, &log.lock().unwrap())
        })
    }
}

type Undo = Arc<Mutex<HashMap<PathBuf, Option<Vec<u8>>>>>;

/// Calls listed after a failure, the last ones.
const RAN_SHOWN: usize = 20;
/// Stored keys named in the footer.
const KEYS_SHOWN: usize = 10;

fn render(mut o: engine::Outcome, undo: Option<Undo>, log: &Log) -> ToolOutput {
    let mut content = Vec::new();
    let mut text = String::new();
    for item in std::mem::take(&mut o.items) {
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
    if log.refused.is_some() {
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str("(the script stopped calling tools after a refused call)");
    }
    if is_error && !log.ran.is_empty() {
        text.push_str("\nCalls that ran:");
        let skip = log.ran.len().saturating_sub(RAN_SHOWN);
        if skip > 0 {
            text.push_str(&format!("\n… {skip} earlier"));
        }
        for l in &log.ran[skip..] {
            text.push('\n');
            text.push_str(l);
        }
    }
    let mut t = text.trim_end().to_string();
    if t.is_empty() && content.is_empty() {
        t = "(no output)".into();
    }
    if !t.is_empty() {
        t.push('\n');
    }
    t.push_str(&footer(&o));
    content.push(Block::text(t));
    ToolOutput {
        content,
        is_error,
        details: Some(json!({ "calls": o.calls, "elapsed_ms": o.elapsed.as_millis() as u64 })),
    }
}

/// `[N calls · 1.2 s · stored: a, b]`
fn footer(o: &engine::Outcome) -> String {
    let mut f = format!(
        "[{} call{} · {:.1} s",
        o.calls,
        if o.calls == 1 { "" } else { "s" },
        o.elapsed.as_secs_f64()
    );
    if !o.store.is_empty() {
        let mut keys: Vec<&str> = o.store.keys().take(KEYS_SHOWN).map(String::as_str).collect();
        if o.store.len() > KEYS_SHOWN {
            keys.push("…");
        }
        f.push_str(&format!(" · stored: {}", keys.join(", ")));
    }
    f.push(']');
    f
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::Permissions;

    fn mcp_tool(server: &str, name: &str, description: &str) -> ToolInfo {
        ToolInfo {
            name: format!("{server}__{name}"),
            description: description.into(),
            schema: json!({ "type": "object", "properties": { "n": { "type": "number" } }, "required": ["n"] }),
            namespace: Some(server.into()),
            output_schema: None,
        }
    }

    #[test]
    fn signatures_go_under_their_server() {
        let servers = [
            ("gh".to_string(), "GitHub".to_string()),
            ("bare".to_string(), String::new()),
        ];
        let long = format!("Find an issue by number. {}", "x".repeat(200));
        let c = Codemode::with_signatures(
            &servers,
            &[mcp_tool("gh", "issue", &long), mcp_tool("other", "x", "no")],
        );
        let d = c.description();
        let tail = d.split_once("\n- gh: GitHub\n  ").unwrap().1;
        let line = tail.lines().next().unwrap();
        assert!(line.starts_with("mcp.gh.issue(args: { n: number })"), "{line}");
        assert!(line.ends_with(" — Find an issue by number"), "{line}");
        assert!(d.ends_with("\n- bare"), "{d}");
        assert!(!d.contains("other"));
        // Without signatures the description is the plain server list.
        assert!(
            Codemode::new(&servers)
                .description()
                .ends_with("\n- gh: GitHub\n- bare")
        );
    }

    #[test]
    fn signatures_past_the_budget_fall_back_to_names_then_nothing() {
        let servers = [
            ("a".to_string(), String::new()),
            ("b".to_string(), String::new()),
            ("c".to_string(), String::new()),
        ];
        let desc = "d".repeat(100);
        let mut tools: Vec<ToolInfo> = (0..30).map(|i| mcp_tool("a", &format!("t{i}"), &desc)).collect();
        // b: names alone (~1,300 chars) fit after a's lines; c's names (~7,000) don't.
        tools.extend((0..200).map(|i| mcp_tool("b", &format!("b{i:03}"), &desc)));
        tools.extend((0..1000).map(|i| mcp_tool("c", &format!("c{i:04}"), &desc)));
        let d = Codemode::with_signatures(&servers, &tools).description().to_string();
        let list = d.split_once("\nMCP servers").unwrap().1;
        assert!(list.contains("\n- a\n  mcp.a.t0("), "{list}");
        assert!(list.contains("\n- b\n  tools: b000, b001, "), "{list}");
        assert!(list.ends_with("\n- c"), "{list}");
        let sig_chars: usize = list
            .lines()
            .filter(|l| l.starts_with("  "))
            .map(|l| l.chars().count() + 1)
            .sum();
        assert!(sig_chars <= SIGNATURE_BUDGET, "{sig_chars}");
    }

    fn text_of(o: &ToolOutput) -> String {
        o.content
            .iter()
            .filter_map(|b| match b {
                Block::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect()
    }

    fn outcome(calls: usize, error: Option<&str>, keys: usize) -> engine::Outcome {
        engine::Outcome {
            items: vec![Item::Text("hi".into())],
            result: None,
            error: error.map(String::from),
            calls,
            store: (0..keys).map(|i| (format!("k{i:02}"), json!(i))).collect(),
            elapsed: Duration::from_millis(1234),
            truncated: false,
        }
    }

    #[test]
    fn failure_lists_the_last_calls_and_every_result_has_a_footer() {
        let log = Log {
            refused: None,
            ran: (0..25).map(|i| format!("✓ read({{\"i\":{i}}}) 1ms")).collect(),
        };
        let out = render(outcome(25, Some("Error: boom"), 12), None, &log);
        let t = text_of(&out);
        assert!(out.is_error);
        assert!(
            t.contains("Script failed: Error: boom\nCalls that ran:\n… 5 earlier\n✓ read({\"i\":5}) 1ms\n"),
            "{t}"
        );
        assert!(!t.contains("{\"i\":4}"));
        assert!(
            t.ends_with("✓ read({\"i\":24}) 1ms\n[25 calls · 1.2 s · stored: k00, k01, k02, k03, k04, k05, k06, k07, k08, k09, …]"),
            "{t}"
        );
        // Success: no call list, a one-call footer without keys.
        let ok = text_of(&render(outcome(1, None, 0), None, &log));
        assert_eq!(ok, "hi\n[1 call · 1.2 s]");
        let refused = Log {
            refused: Some("denied".into()),
            ran: vec![],
        };
        let r = text_of(&render(outcome(2, None, 0), None, &refused));
        assert_eq!(
            r,
            "hi\n(the script stopped calling tools after a refused call)\n[2 calls · 1.2 s]"
        );
    }

    struct Sleepy(&'static str, bool, u64);
    impl Tool for Sleepy {
        fn name(&self) -> &str {
            self.0
        }
        fn description(&self) -> &str {
            "sleeps"
        }
        fn schema(&self) -> Value {
            json!({ "type": "object", "properties": {} })
        }
        fn read_only(&self) -> bool {
            self.1
        }
        fn execute<'a>(&'a self, _args: Value, _ctx: &'a ToolCtx) -> BoxFuture<'a, ToolOutput> {
            Box::pin(async {
                tokio::time::sleep(Duration::from_millis(self.2)).await;
                ToolOutput::text("ok")
            })
        }
    }

    struct SlowYes;
    impl Approver for SlowYes {
        fn approve(&self, _req: ApprovalRequest) -> BoxFuture<'_, Approval> {
            Box::pin(async {
                tokio::time::sleep(Duration::from_millis(80)).await;
                Approval::Once
            })
        }
    }

    #[test]
    fn images_ride_along_with_structured_content() {
        let img = || vec![json!({ "type": "image", "data": "x", "mimeType": "image/png" })];
        let s = json!({ "sum": 3 });
        assert_eq!(script_value("t".into(), vec![], Some(&s)), s);
        assert_eq!(
            script_value("t".into(), img(), Some(&s)),
            json!({ "sum": 3, "images": img() })
        );
        let has = json!({ "images": 1 });
        assert_eq!(
            script_value("t".into(), img(), Some(&has)),
            json!({ "structured": has, "images": img() })
        );
        assert_eq!(
            script_value("t".into(), img(), Some(&json!([1]))),
            json!({ "structured": [1], "images": img() })
        );
        assert_eq!(script_value("t".into(), vec![], None), json!("t"));
        assert_eq!(
            script_value("t".into(), img(), None),
            json!({ "text": "t", "images": img() })
        );
    }

    /// Records that it ran.
    struct Marks(Arc<AtomicBool>);
    impl Tool for Marks {
        fn name(&self) -> &str {
            "mark"
        }
        fn description(&self) -> &str {
            "marks"
        }
        fn schema(&self) -> Value {
            json!({ "type": "object", "properties": {} })
        }
        fn read_only(&self) -> bool {
            true
        }
        fn execute<'a>(&'a self, _args: Value, _ctx: &'a ToolCtx) -> BoxFuture<'a, ToolOutput> {
            self.0.store(true, Ordering::SeqCst);
            Box::pin(async { ToolOutput::text("ok") })
        }
    }

    /// An allowed call on the script's stopped token is refused and doesn't run.
    #[tokio::test]
    async fn allowed_call_after_the_stop_does_not_run() {
        let dir = tempfile::tempdir().unwrap();
        let ran = Arc::new(AtomicBool::new(false));
        let runner = Runner {
            tools: vec![Arc::new(Marks(ran.clone()))],
            policy: Arc::new(Policy::new(Permissions::default(), dir.path().into(), None)),
            approver: None,
            spill_dir: dir.path().into(),
            redactor: Redactor::default(),
            budget: Budget {
                window_tokens: 100_000,
                used_tokens: 0,
            },
            seq: Default::default(),
            hooks: Default::default(),
            judge: None,
            wait: Default::default(),
        };
        let ctx = ToolCtx {
            cwd: dir.path().into(),
            env: Arc::new(novacode_tools::LocalEnv),
            cancel: Default::default(),
            updates: None,
            files: Default::default(),
            session_id: "t".into(),
            processes: None,
            native_tools: false,
            nested: None,
            script: true,
        };
        ctx.cancel.cancel();
        let out = runner.call("mark".into(), json!({}), ctx).await;
        assert!(out.is_error);
        assert_eq!(out.details.as_ref().unwrap()["refused"], true);
        assert!(!ran.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn approval_cards_and_ask_user_count_as_user_wait() {
        let dir = tempfile::tempdir().unwrap();
        let runner = Runner {
            tools: vec![
                Arc::new(Sleepy("ask_user", true, 60)),
                Arc::new(Sleepy("zap", false, 400)),
            ],
            policy: Arc::new(Policy::new(
                serde_json::from_value::<Permissions>(json!({ "ask": ["zap"] })).unwrap(),
                dir.path().into(),
                None,
            )),
            approver: Some(Arc::new(SlowYes)),
            spill_dir: dir.path().into(),
            redactor: Redactor::default(),
            budget: Budget {
                window_tokens: 100_000,
                used_tokens: 0,
            },
            seq: Default::default(),
            hooks: Default::default(),
            judge: None,
            wait: Default::default(),
        };
        let ctx = ToolCtx {
            cwd: dir.path().into(),
            env: Arc::new(novacode_tools::LocalEnv),
            cancel: Default::default(),
            updates: None,
            files: Default::default(),
            session_id: "t".into(),
            processes: None,
            native_tools: false,
            nested: None,
            script: true,
        };
        assert_eq!(runner.user_wait(), Duration::ZERO);
        let out = runner.call("ask_user".into(), json!({}), ctx.clone()).await;
        assert!(!out.is_error, "{:?}", out.content);
        let asked = runner.user_wait();
        assert!(asked >= Duration::from_millis(60), "{asked:?}");
        let out = runner.call("zap".into(), json!({}), ctx).await;
        assert!(!out.is_error, "{:?}", out.content);
        // The approval (80 ms) counts, the tool's own run (400 ms) doesn't.
        let approved = runner.user_wait() - asked;
        assert!(
            approved >= Duration::from_millis(80) && approved < Duration::from_millis(400),
            "{approved:?}"
        );
    }
}
