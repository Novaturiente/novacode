//! Extension API v1 surface snapshot (L7.11, binding principle 7). Any change to
//! what extensions see fails here: the `novacode` object, hook payloads, tool and
//! command calls, UI ops and the process-extension protocol. v1 is additive-only:
//! an addition is accepted with `UPDATE_SNAPSHOTS=1`; a rename or removal needs v2.
//! Session events (`on("agent_start")`, …) are the wire events, snapshotted in
//! `crates/novacode/tests/wire.snap.jsonl`.

use novacode_core::extensions::Extensions;
use novacode_core::hooks::Handler;
use novacode_core::process_ext::Config;
use novacode_tools::{ToolCtx, ToolOutput};
use novacode_types::ui::{Line, Node, UiOp};
use serde_json::{Value, json};
use std::sync::Arc;

const RECORDER: &str = r#"
const seen = {};
const keys = (v) => (v && typeof v === "object" && !Array.isArray(v) ? Object.keys(v).sort() : typeof v);
const walk = (o, p, out) => {
  for (const k of Object.keys(o).sort()) {
    const v = o[k];
    if (v && typeof v === "object" && !Array.isArray(v)) walk(v, `${p}.${k}`, out);
    else out.push(`${p}.${k}: ${typeof v === "function" ? `function/${v.length}` : typeof v}`);
  }
  return out;
};
export default (nc) => {
  for (const e of ["before_tool", "after_tool", "input", "before_request", "ui_key"]) nc.on(e, (p) => { seen[e] = keys(p); });
  nc.registerTool({ name: "surface", execute: () => walk(nc, "novacode", []).join("\n") });
  nc.registerTool({ name: "probe", execute: (args, ctx) => { seen.tool = { args: keys(args), ctx: keys(ctx) }; return "ok"; } });
  nc.registerTool({ name: "seen", execute: () => JSON.stringify(seen) });
  nc.registerCommand("probe", { handler: (args) => { seen.command = typeof args; } });
};
"#;

/// Answers every request, writing what it got to `log`, one JSON line each.
const PROCESS: &str = r#"
import json, sys
log = open(sys.argv[1], "a")
def keys(v): return sorted(v) if isinstance(v, dict) else type(v).__name__
for line in sys.stdin:
    m = json.loads(line)
    p = m.get("params", {})
    log.write(json.dumps({"method": m["method"], "params": keys(p),
        "payload": keys(p.get("payload", p.get("args")))}) + "\n"); log.flush()
    if "id" not in m: continue
    r = {"tools": [{"name": "ptool", "description": "", "parameters": {"type": "object"}}],
         "commands": [{"name": "pcmd", "description": ""}], "events": ["before_tool"]} if m["method"] == "initialize" else None
    print(json.dumps({"jsonrpc": "2.0", "id": m["id"], "result": r}), flush=True)
"#;

fn ctx(cwd: &std::path::Path) -> ToolCtx {
    ToolCtx {
        cwd: cwd.into(),
        env: Arc::new(novacode_tools::LocalEnv),
        cancel: Default::default(),
        updates: None,
        files: Default::default(),
        session_id: "s".into(),
        processes: None,
        native_tools: false,
        nested: None,
        script: false,
    }
}

fn text(o: &ToolOutput) -> String {
    o.content
        .iter()
        .filter_map(|b| match b {
            novacode_types::Block::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

/// One of each UI op; the match makes a new variant a compile error here.
fn ui_ops() -> Vec<UiOp> {
    let line = || vec![Line::Plain("x".into())];
    let ops = vec![
        UiOp::Status {
            key: "k".into(),
            text: Some("t".into()),
            style: None,
        },
        UiOp::Widget {
            key: "k".into(),
            lines: Some(line()),
            below: false,
        },
        UiOp::Footer { lines: Some(line()) },
        UiOp::Header { lines: Some(line()) },
        UiOp::Editor {
            title: None,
            border: None,
        },
        UiOp::OpenTab {
            name: "t".into(),
            title: None,
            priority: 0,
            width: None,
            region: false,
            show: false,
        },
        UiOp::ShowTab { name: "t".into() },
        UiOp::CloseTab { name: "t".into() },
        UiOp::Source {
            tab: "t".into(),
            id: "s".into(),
            title: None,
            order: 0.0,
            newest_first: false,
        },
        UiOp::Publish {
            tab: "t".into(),
            source: "s".into(),
            node: Box::new(serde_json::from_value::<Node>(json!({"id": "n", "label": "l"})).unwrap()),
        },
        UiOp::Remove {
            tab: "t".into(),
            source: "s".into(),
            id: None,
        },
        UiOp::Region {
            tab: "t".into(),
            lines: line(),
        },
        UiOp::Shortcut {
            key: "k".into(),
            description: String::new(),
        },
    ];
    for op in &ops {
        match op {
            UiOp::Status { .. }
            | UiOp::Widget { .. }
            | UiOp::Footer { .. }
            | UiOp::Header { .. }
            | UiOp::Editor { .. }
            | UiOp::OpenTab { .. }
            | UiOp::ShowTab { .. }
            | UiOp::CloseTab { .. }
            | UiOp::Source { .. }
            | UiOp::Publish { .. }
            | UiOp::Remove { .. }
            | UiOp::Region { .. }
            | UiOp::Shortcut { .. } => {}
        }
    }
    ops
}

#[tokio::test(flavor = "multi_thread")]
async fn api_v1_surface() {
    let d = tempfile::tempdir().unwrap();
    let dir = d.path().join("ext");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("rec.js"), RECORDER).unwrap();
    let py = d.path().join("p.py");
    std::fs::write(&py, PROCESS).unwrap();
    let log = d.path().join("rpc.log");
    let procs = vec![(
        "proc".to_string(),
        Config {
            command: "python3".into(),
            args: vec![py.display().to_string(), log.display().to_string()],
            env: Default::default(),
        },
    )];
    let exts = Extensions::load_all(
        std::slice::from_ref(&dir),
        &procs,
        d.path(),
        d.path(),
        &d.path().join("cache"),
        "s",
        d.path(),
        json!({}),
    )
    .await;
    assert!(exts.failed.is_empty(), "{:?}", exts.failed);
    let tools = exts.tools();
    let tool = |n: &str| tools.iter().find(|t| t.name() == n).unwrap().clone();
    let c = ctx(d.path());

    // Drive every entry point once.
    exts.before_tool("read", &json!({"path": "a"})).await;
    let run = novacode_core::hooks::Run {
        call_id: "c1".into(),
        duration_ms: 5,
        touched: vec![d.path().join("a")],
    };
    exts.after_tool("read", &json!({"path": "a"}), &ToolOutput::text("out"), &run)
        .await;
    exts.input("hi", false).await;
    exts.before_request("p", "m", 1, Some("hi".into())).await;
    exts.ui_key(
        "rec",
        json!({"tab": null, "node": null, "key": "x", "width": 1, "height": 1}),
    )
    .await;
    tool("probe").execute(json!({"a": 1}), &c).await;
    tool("ptool").execute(json!({"a": 1}), &c).await;
    exts.command("/probe x").await;
    exts.command("/pcmd x").await;

    let mut out =
        String::from("# novacode extension API v1 (generated by tests/api_v1.rs)\n\n## script: the novacode object\n");
    out += &text(&tool("surface").execute(json!({}), &c).await);
    out += "\n\n## script: what handlers receive (payload keys)\n";
    let seen: Value = serde_json::from_str(&text(&tool("seen").execute(json!({}), &c).await)).unwrap();
    for (k, v) in seen.as_object().unwrap() {
        out += &format!("{k}: {v}\n");
    }
    out += "\n## ui ops (novacode.ui and process `ui` notifications)\n";
    for op in ui_ops() {
        out += &format!("{}\n", serde_json::to_string(&op).unwrap());
    }
    out += "\n## process extensions: requests sent (method, params keys, payload keys)\n";
    let mut lines: Vec<String> = std::fs::read_to_string(&log)
        .unwrap()
        .lines()
        .map(String::from)
        .collect();
    lines.sort();
    lines.dedup();
    for l in lines {
        out += &format!("{l}\n");
    }

    out += "\n## provider programs: requests sent (method, params keys)\n";
    // The fake provider is Python: text mode writes `\r\n` on Windows.
    out += &provider_requests(d.path()).await.replace("\r\n", "\n");

    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/api-v1.snap");
    if std::env::var_os("UPDATE_SNAPSHOTS").is_some() || !path.exists() {
        std::fs::write(&path, &out).unwrap();
        return;
    }
    let want = std::fs::read_to_string(&path).unwrap();
    assert_eq!(
        out, want,
        "extension API v1 changed. Additions: UPDATE_SNAPSHOTS=1 and document them in docs/extensions.md. Renames or removals need API v2 (DECISIONS 2026-10-19)."
    );
}

/// Records the args it was called with and answers like the real agent tool's spawn.
struct FakeAgentTool(std::sync::Mutex<Vec<Value>>);
impl novacode_tools::Tool for FakeAgentTool {
    fn name(&self) -> &str {
        "agent"
    }
    fn description(&self) -> &str {
        "fake"
    }
    fn schema(&self) -> Value {
        json!({"type": "object"})
    }
    fn execute<'a>(&'a self, args: Value, _: &'a ToolCtx) -> futures_util::future::BoxFuture<'a, ToolOutput> {
        self.0.lock().unwrap().push(args);
        Box::pin(async {
            ToolOutput {
                details: Some(json!({"id": "child1"})),
                ..ToolOutput::text("started child1")
            }
        })
    }
}

/// `session.messages()`, `complete()` and `spawnAgent()` reach the live session (L7.11).
#[tokio::test(flavor = "multi_thread")]
async fn session_complete_and_spawn() {
    use novacode_ai::faux::{Faux, FauxReply};
    use novacode_core::agent::{self, Agent};
    use novacode_types::{AgentEvent, Message};
    let work = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let dir = home.path().join("ext");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("e.js"),
        r#"export default (nc) => nc.registerCommand("go", { handler: async () => {
             const m = await nc.session.messages();
             const c = await nc.complete({ system: "be short", messages: [{ role: "user", content: "sum up" }], maxTokens: 50 });
             const s = await nc.spawnAgent("review it", { notify: "quiet" });
             await nc.state.append("note", { n: 1 });
             await nc.state.append("note", { n: 2 });
             const st = await nc.state.entries("note");
             const mine = await nc.settings.get("extensions.e.color");
             await nc.sendMessage("follow up please");
             return `${m.length} ${m[0].role} | ${c.text} | ${s.id} | ${st.map((x) => x.n).join(",")} ${mine}`;
           } });"#,
    )
    .unwrap();
    let faux = Faux::new([
        FauxReply::text("hello there"),
        FauxReply::text("a summary"),
        FauxReply::text("followed up"),
    ]);
    let paths = novacode_base::paths::Paths::new(home.path());
    let store = novacode_core::session::Store::create(&paths, work.path(), false);
    let exts = Extensions::load_all(
        std::slice::from_ref(&dir),
        &[],
        work.path(),
        home.path(),
        &home.path().join("cache"),
        "",
        home.path(),
        json!({"extensions": {"e": {"color": "teal"}}}),
    )
    .await;
    let fake = Arc::new(FakeAgentTool(Default::default()));
    let mut tools = novacode_tools::builtins(false);
    tools.push(fake.clone());
    let cfg = novacode_core::agent::Config {
        cwd: work.path().into(),
        provider: "faux".into(),
        model: "m".into(),
        max_tokens: 1000,
        cache: novacode_ai::adapter::CacheRetention::Short,
        window_tokens: 200_000,
        images: true,
        reasoning: false,
        thinking: Default::default(),
        spill_dir: home.path().join("spill"),
        redactor: Default::default(),
        prompt: novacode_core::prompt::Sources {
            cwd: work.path().into(),
            home: home.path().into(),
            context_files: false,
            skill_dirs: vec![],
            tool_guidelines: vec![],
            agent: None,
            memory_index: None,
            mode: None,
        },
        warm: false,
        metrics: false,
    };
    let agent = Agent::new(
        Arc::new(faux.clone()),
        tools,
        store,
        cfg,
        Arc::new(novacode_tools::LocalEnv),
    )
    .with_extensions(exts.clone());
    let (h, task) = agent::spawn(agent);
    let mut rx = h.subscribe();
    h.prompt("hi");
    while !matches!(rx.recv().await.unwrap(), AgentEvent::AgentEnd { .. }) {}
    let out = exts.command("/go").await.unwrap().unwrap();
    // sendMessage (followUp) starts a run on the idle session.
    while !matches!(rx.recv().await.unwrap(), AgentEvent::AgentEnd { .. }) {}
    h.shutdown();
    let agent = task.await.unwrap();

    assert_eq!(out, "2 user | a summary | child1 | 1,2 teal");
    let saved: Vec<_> = agent
        .store
        .entries()
        .iter()
        .filter_map(|e| match &e.kind {
            novacode_core::session::EntryKind::ExtState { ext, kind, data } => {
                Some((ext.clone(), kind.clone(), data["n"].clone()))
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        saved,
        [
            ("e".into(), "note".into(), json!(1)),
            ("e".into(), "note".into(), json!(2))
        ]
    );
    assert!(
        agent.store.projection().iter().any(|m| matches!(m, Message::User { content } if content.iter().any(|b| matches!(b, novacode_types::Block::Text { text, .. } if text == "follow up please")))),
        "the message was sent as the user's"
    );
    let req = faux.requests.lock().unwrap()[1].clone();
    assert!(
        matches!(&req.messages[0], Message::System { text } if text == "be short"),
        "{:?}",
        req.messages
    );
    assert_eq!(
        req.messages.len(),
        2,
        "only the extension's messages, not the session's"
    );
    assert!(req.tools.is_empty() && req.max_tokens == 50);
    let args = fake.0.lock().unwrap()[0].clone();
    assert_eq!(args["action"], "spawn");
    assert_eq!(args["task"], "review it");
    assert_eq!(args["notify"], "quiet");
    assert_eq!(args["title"], "[ext:e] review it", "tagged with the extension's name");
}

/// A provider program (L7.6) that logs what it is sent and answers with one text event.
const PROVIDER: &str = r#"
import json, sys
log = open(sys.argv[1], "a")
for line in sys.stdin:
    m = json.loads(line)
    log.write(json.dumps({"method": m["method"], "params": sorted(m.get("params", {}))}) + "\n"); log.flush()
    if "id" not in m: continue
    if m["method"] == "stream":
        print(json.dumps({"jsonrpc": "2.0", "method": "event", "params": {"id": m["id"], "event": {"type": "text", "delta": "ok"}}}), flush=True)
    print(json.dumps({"jsonrpc": "2.0", "id": m["id"], "result": {"models": []} if m["method"] == "initialize" else {"stop": "stop"}}), flush=True)
"#;

async fn provider_requests(dir: &std::path::Path) -> String {
    use novacode_ai::adapter::{Adapter, CacheRetention, Request, collect};
    let f = dir.join("prov.py");
    let log = dir.join("prov.log");
    std::fs::write(&f, PROVIDER).unwrap();
    let p = novacode_ai::process::Process::new(
        "mine",
        serde_json::from_value(json!({"command": "python3", "args": [f, log]})).unwrap(),
    );
    let req = Request {
        model: "m".into(),
        messages: vec![novacode_types::Message::user("hi")],
        tools: vec![],
        max_tokens: 10,
        cache: CacheRetention::Short,
        thinking: None,
        images: false,
        session: None,
        tail: vec![],
    };
    let m = collect(p.stream(req, Default::default())).await;
    assert_eq!(m.text(), "ok");
    std::fs::read_to_string(&log).unwrap()
}
