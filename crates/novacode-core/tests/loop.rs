//! M2 checks: scripted faux sessions with tools, the cache-prefix test (L1.7
//! rule 6), retry hiding, cancellation, and resume from disk.

use novacode_ai::adapter::{CacheRetention, Request};
use novacode_ai::anthropic::build_body;
use novacode_ai::faux::{Faux, FauxReply};
use novacode_base::paths::Paths;
use novacode_core::agent::{self, Agent, Config};
use novacode_core::session::Store;
use novacode_tools::LocalEnv;
use novacode_types::{AgentEvent, Block, ErrorKind, Message, StopReason, ToolCall, Usage};
use serde_json::{Value, json};
use std::path::Path;
use std::sync::Arc;

fn call(id: &str, name: &str, args: Value) -> Block {
    Block::ToolCall(ToolCall {
        id: id.into(),
        name: name.into(),
        arguments: args,
        arguments_error: None,
        replay: None,
    })
}

fn tools_reply(blocks: Vec<Block>) -> FauxReply {
    FauxReply::Message {
        blocks,
        stop: StopReason::ToolUse,
        usage: Usage {
            input: 10,
            output: 5,
            ..Default::default()
        },
    }
}

fn cfg(cwd: &Path, home: &Path) -> Config {
    Config {
        cwd: cwd.into(),
        provider: "faux".into(),
        model: "m".into(),
        max_tokens: 1000,
        cache: CacheRetention::Short,
        window_tokens: 200_000,
        images: true,
        reasoning: true,
        thinking: Default::default(),
        spill_dir: home.join("spill"),
        redactor: Default::default(),
        prompt: novacode_core::prompt::Sources {
            cwd: cwd.into(),
            home: home.into(),
            context_files: true,
            skill_dirs: vec![],
            tool_guidelines: vec![],
            agent: None,
            memory_index: None,
            mode: None,
        },
        warm: false,
        metrics: false,
    }
}

/// Send prompts one by one, waiting for each run to settle. Returns all events.
async fn drive(agent: Agent, prompts: &[&str]) -> (Agent, Vec<AgentEvent>) {
    drive_with(agent, prompts, |_| {}).await
}

/// `drive`, after `before` ran on the session handle.
async fn drive_with(
    agent: Agent,
    prompts: &[&str],
    before: impl FnOnce(&agent::SessionHandle),
) -> (Agent, Vec<AgentEvent>) {
    let (h, task) = agent::spawn(agent);
    before(&h);
    let mut rx = h.subscribe();
    let mut events = Vec::new();
    for p in prompts {
        h.prompt(*p);
        loop {
            let ev = rx.recv().await.unwrap();
            let done = matches!(ev, AgentEvent::AgentEnd { .. });
            events.push(ev);
            if done {
                break;
            }
        }
    }
    h.shutdown();
    (task.await.unwrap(), events)
}

/// Anthropic body bytes per message with cache markers removed (markers move by design).
fn wire_messages(r: &Request) -> Vec<String> {
    let mut body = build_body(r, None, "anthropic");
    fn strip(v: &mut Value) {
        match v {
            Value::Object(o) => {
                o.remove("cache_control");
                o.values_mut().for_each(strip);
            }
            Value::Array(a) => a.iter_mut().for_each(strip),
            _ => {}
        }
    }
    strip(&mut body);
    body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m.to_string())
        .collect()
}

#[tokio::test]
async fn tools_session_and_cache_prefix() {
    let work = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    std::fs::write(work.path().join("notes.txt"), "alpha\nbeta\n").unwrap();
    let faux = Faux::new([
        // Turn 1: two tools in parallel, then an edit, then text.
        tools_reply(vec![
            Block::text("Looking."),
            call("c1", "read", json!({"path": "notes.txt"})),
            call("c2", "grep", json!({"pattern": "beta"})),
        ]),
        tools_reply(vec![call(
            "c3",
            "edit",
            json!({"path": "notes.txt", "edits": [{"old_text": "beta", "new_text": "gamma"}]}),
        )]),
        FauxReply::text("Done: beta is now gamma."),
        // Turn 2: an unknown tool (rejected, suggestion), then bash.
        tools_reply(vec![call("c4", "Bash", json!({"command": "echo hi"}))]),
        tools_reply(vec![call("c5", "bash", json!({"command": "cat notes.txt"}))]),
        FauxReply::text("It says alpha and gamma."),
    ]);
    let paths = Paths::new(home.path());
    let store = Store::create(&paths, work.path(), false);
    let agent = Agent::new(
        Arc::new(faux.clone()),
        novacode_tools::builtins(false),
        store,
        cfg(work.path(), home.path()),
        Arc::new(LocalEnv),
    );
    let (agent, events) = drive(agent, &["rename beta", "what does it say?"]).await;

    assert_eq!(
        std::fs::read_to_string(work.path().join("notes.txt")).unwrap(),
        "alpha\ngamma\n"
    );
    let ends: Vec<_> = events
        .iter()
        .filter(|e| matches!(e, AgentEvent::AgentEnd { .. }))
        .collect();
    assert_eq!(ends, vec![&AgentEvent::AgentEnd { stop: StopReason::Stop }; 2]);

    let reqs = faux.requests.lock().unwrap().clone();
    assert_eq!(reqs.len(), 6);
    // Tool results come back in source order, each matching its call.
    let r2 = &reqs[1].messages;
    let ids: Vec<&str> = r2
        .iter()
        .filter_map(|m| match m {
            Message::ToolResult { call_id, .. } => Some(call_id.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(ids, ["c1", "c2"]);
    let unknown = format!("{:?}", reqs[4].messages.last().unwrap());
    assert!(unknown.contains("Did you mean `bash`?"), "{unknown}");

    // THE CACHE-PREFIX TEST: every request is a byte prefix of the next one,
    // with the same system prompt and tools.
    for w in reqs.windows(2) {
        assert_eq!(w[0].tools, w[1].tools);
        let (a, b) = (wire_messages(&w[0]), wire_messages(&w[1]));
        assert!(
            b.len() > a.len() && b[..a.len()] == a[..],
            "request is not a prefix of the next"
        );
        assert_eq!(
            build_body(&w[0], None, "anthropic")["system"],
            build_body(&w[1], None, "anthropic")["system"]
        );
    }

    // Resume from disk: the projection equals what was sent plus the final reply.
    let path = agent.store.path().unwrap().to_path_buf();
    let live = agent.store.projection();
    drop(agent);
    let reopened = Store::open(&path).unwrap();
    assert_eq!(reopened.projection(), live);
    let mut sent = reqs[5].messages.clone();
    sent.push(live.last().unwrap().clone());
    assert_eq!(live, sent, "stored = sent");
}

#[tokio::test]
async fn retry_is_hidden_and_errors_end_the_run() {
    let work = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let faux = Faux::new([
        FauxReply::Fail(ErrorKind::Transient, "stream cut".into()),
        FauxReply::text("ok"),
        FauxReply::Fail(ErrorKind::Auth, "bad token".into()),
    ]);
    let store = Store::create(&Paths::new(home.path()), work.path(), true);
    let agent = Agent::new(
        Arc::new(faux.clone()),
        vec![],
        store,
        cfg(work.path(), home.path()),
        Arc::new(LocalEnv),
    )
    .with_retry_base(std::time::Duration::from_millis(5));
    let (_agent, events) = drive(agent, &["hi", "again"]).await;
    assert!(events.iter().any(|e| matches!(
        e,
        AgentEvent::Retry {
            attempt: 1,
            error: ErrorKind::Transient,
            ..
        }
    )));
    let reqs = faux.requests.lock().unwrap();
    assert_eq!(
        reqs[0].messages, reqs[1].messages,
        "the failed attempt is hidden from the retry"
    );
    assert!(events.contains(&AgentEvent::AgentEnd {
        stop: StopReason::Error { kind: ErrorKind::Auth }
    }));
    // The run after the error sees "hi", "ok", "again" and not the hidden failure.
    assert_eq!(
        reqs[2]
            .messages
            .iter()
            .filter(|m| matches!(m, Message::Assistant(_)))
            .count(),
        1
    );
}

#[tokio::test]
async fn abort_gives_every_call_a_result() {
    let work = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let faux = Faux::new([
        tools_reply(vec![
            call("s1", "bash", json!({"command": "sleep 30"})),
            call("s2", "ls", json!({})),
        ]),
        FauxReply::text("next"),
    ]);
    let store = Store::create(&Paths::new(home.path()), work.path(), true);
    let agent = Agent::new(
        Arc::new(faux.clone()),
        novacode_tools::builtins(false),
        store,
        cfg(work.path(), home.path()),
        Arc::new(LocalEnv),
    );
    let (h, task) = agent::spawn(agent);
    let mut rx = h.subscribe();
    h.prompt("sleep");
    loop {
        if let AgentEvent::ToolStart { call } = rx.recv().await.unwrap()
            && call.id == "s1"
        {
            break;
        }
    }
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    h.abort();
    let t0 = std::time::Instant::now();
    while !matches!(rx.recv().await.unwrap(), AgentEvent::AgentEnd { .. }) {}
    assert!(t0.elapsed().as_secs() < 5);
    // The next prompt works and its history has a result for both calls.
    h.prompt("go on");
    while !matches!(rx.recv().await.unwrap(), AgentEvent::AgentEnd { .. }) {}
    h.shutdown();
    task.await.unwrap();
    let reqs = faux.requests.lock().unwrap();
    let results = reqs[1]
        .messages
        .iter()
        .filter(|m| matches!(m, Message::ToolResult { .. }))
        .count();
    assert_eq!(results, 2);
}

#[tokio::test]
async fn cut_off_and_empty_replies_are_nudged_within_caps() {
    let work = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let cut = |t: &str| FauxReply::Message {
        blocks: vec![Block::text(t)],
        stop: StopReason::Length,
        usage: Usage::default(),
    };
    let faux = Faux::new([cut("a"), cut("b"), cut("c"), FauxReply::text("unused")]);
    let store = Store::create(&Paths::new(home.path()), work.path(), true);
    let agent = Agent::new(
        Arc::new(faux.clone()),
        vec![],
        store,
        cfg(work.path(), home.path()),
        Arc::new(LocalEnv),
    );
    let (_a, events) = drive(agent, &["write a lot"]).await;
    assert_eq!(faux.requests.lock().unwrap().len(), 3, "two continues, then stop");
    assert!(events.contains(&AgentEvent::AgentEnd {
        stop: StopReason::Length
    }));
    let last = faux.requests.lock().unwrap()[2].messages.last().cloned().unwrap();
    assert!(format!("{last:?}").contains("<system-reminder>"));
}

/// Approves everything for the session and counts the cards.
struct Yes(std::sync::Mutex<Vec<String>>);
impl novacode_core::policy::Approver for Yes {
    fn approve(
        &self,
        req: novacode_core::policy::ApprovalRequest,
    ) -> futures_util::future::BoxFuture<'_, novacode_core::policy::Approval> {
        self.0.lock().unwrap().push(req.rule);
        Box::pin(async { novacode_core::policy::Approval::Session })
    }
}

fn result_text(m: &Message) -> String {
    match m {
        Message::ToolResult { content, .. } => content
            .iter()
            .filter_map(|b| match b {
                Block::Text { text, .. } => Some(text.clone()),
                _ => None,
            })
            .collect(),
        _ => String::new(),
    }
}

/// M3 check: the policy runs inside the loop for every call; nothing reaches a tool unchecked.
#[tokio::test]
async fn policy_gates_every_call_in_the_loop() {
    use novacode_core::policy::{Permissions, Policy};
    let work = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    std::fs::create_dir(work.path().join("target")).unwrap();
    std::fs::write(work.path().join("cfg.txt"), "token=SECRET-TOKEN-123456\n").unwrap();
    let faux = Faux::new([
        tools_reply(vec![
            call("a", "bash", json!({"command": "rm -rf ~"})),
            call("b", "bash", json!({"command": "rm -rf target"})),
            call("c", "bash", json!({"command": "git push origin main"})),
            call("d", "read", json!({"path": "cfg.txt"})),
        ]),
        tools_reply(vec![call(
            "e",
            "bash",
            json!({"command": "rm -rf target", "justification": "build output, cargo recreates it"}),
        )]),
        FauxReply::text("done"),
    ]);
    let perms: Permissions = serde_json::from_value(json!({ "deny": ["Bash(git push:*)"] })).unwrap();
    let mut c = cfg(work.path(), home.path());
    c.redactor = novacode_base::redact::Redactor::new(|| vec![("SECRET-TOKEN-123456".into(), "claude login".into())]);
    let agent = Agent::new(
        Arc::new(faux.clone()),
        novacode_tools::builtins(false),
        Store::create(&Paths::new(home.path()), work.path(), true),
        c,
        Arc::new(LocalEnv),
    )
    .with_policy(Policy::new(perms, home.path().into(), None));
    let (_a, _events) = drive(agent, &["clean up"]).await;

    let reqs = faux.requests.lock().unwrap();
    let r: Vec<String> = reqs[1].messages.iter().rev().take(4).rev().map(result_text).collect();
    assert!(r[0].contains("can never run"), "{}", r[0]);
    // Auto mode never asks (revision L6.2): the risky-but-ordinary command just runs.
    assert!(!r[1].contains("safety check") && !r[1].contains("Held"), "{}", r[1]);
    assert!(r[2].contains("git push"), "{}", r[2]);
    assert!(
        r[3].contains("[redacted: claude login]") && !r[3].contains("SECRET-TOKEN"),
        "{}",
        r[3]
    );
    assert!(!work.path().join("target").exists());
}

/// L4.16: while the session file can't be written, no tool runs and no model is
/// called; once it can, the missing entries are written and the session goes on.
#[tokio::test]
async fn unsaved_session_stops_work_until_it_saves() {
    use std::os::unix::fs::PermissionsExt;
    let work = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let faux = Faux::new([
        FauxReply::text("one"),
        tools_reply(vec![call("a", "bash", json!({"command": "touch made"}))]),
        FauxReply::text("three"),
    ]);
    let store = Store::create(&Paths::new(home.path()), work.path(), false);
    let file = store.path().unwrap().to_path_buf();
    let agent = Agent::new(
        Arc::new(faux.clone()),
        novacode_tools::builtins(false),
        store,
        cfg(work.path(), home.path()),
        Arc::new(LocalEnv),
    );
    let (h, task) = agent::spawn(agent);
    let mut rx = h.subscribe();
    let mut notices = Vec::new();
    async fn run(
        h: &agent::SessionHandle,
        rx: &mut tokio::sync::broadcast::Receiver<AgentEvent>,
        notices: &mut Vec<String>,
        prompt: &str,
    ) {
        h.prompt(prompt);
        loop {
            match rx.recv().await.unwrap() {
                AgentEvent::Stream(novacode_types::StreamEvent::Notice { text }) => notices.push(text),
                AgentEvent::AgentEnd { .. } => return,
                _ => {}
            }
        }
    }
    let mode = |m: u32| std::fs::set_permissions(&file, std::fs::Permissions::from_mode(m)).unwrap();
    run(&h, &mut rx, &mut notices, "first").await;
    mode(0o444);
    run(&h, &mut rx, &mut notices, "make a file").await;
    assert!(!work.path().join("made").exists(), "no tool runs while unsaved");
    assert!(
        notices.iter().any(|n| n.starts_with("session not saved")),
        "{notices:?}"
    );
    run(&h, &mut rx, &mut notices, "again").await;
    // The prompt itself failed to save, so neither run reached the model.
    assert_eq!(faux.requests.lock().unwrap().len(), 1, "no model call while unsaved");
    mode(0o644);
    run(&h, &mut rx, &mut notices, "now").await;
    assert!(notices.iter().any(|n| n == "session saved again"), "{notices:?}");
    h.shutdown();
    drop(task.await.unwrap());
    let users = Store::open(&file)
        .unwrap()
        .projection()
        .iter()
        .filter(|m| matches!(m, Message::User { .. }))
        .count();
    assert!(users >= 4, "every prompt is in the file: {users}");
}

/// Read-only mode: a write through `process` start is refused like one through `bash` (revision L6.1).
#[tokio::test]
async fn read_only_mode_refuses_writes_through_process() {
    use novacode_core::policy::{Mode, Permissions, Policy};
    let work = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let faux = Faux::new([
        tools_reply(vec![
            call(
                "a",
                "process",
                json!({"action": "start", "command": "touch made-by-process"}),
            ),
            call("b", "bash", json!({"command": "touch made-by-bash"})),
        ]),
        FauxReply::text("done"),
    ]);
    let mut tools = novacode_tools::builtins(false);
    tools.push(Arc::new(novacode_tools::process::ProcessTool));
    let agent = Agent::new(
        Arc::new(faux.clone()),
        tools,
        Store::create(&Paths::new(home.path()), work.path(), true),
        cfg(work.path(), home.path()),
        Arc::new(LocalEnv),
    )
    .with_processes(novacode_tools::process::ProcessManager::new(
        &home.path().join("processes"),
        "s",
    ))
    .with_policy(Policy::new(Permissions::default(), home.path().into(), None).with_mode(Mode::ReadOnly));
    drive(agent, &["make files"]).await;
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert!(!work.path().join("made-by-process").exists());
    assert!(!work.path().join("made-by-bash").exists());
    let reqs = faux.requests.lock().unwrap();
    let r: Vec<String> = reqs[1].messages.iter().rev().take(2).rev().map(result_text).collect();
    assert!(r.iter().all(|t| t.contains("read-only shell commands")), "{r:?}");
}

#[tokio::test]
async fn ask_mode_uses_the_approver_once_per_rule() {
    use novacode_core::policy::{Mode, Permissions, Policy};
    let work = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let faux = Faux::new([
        tools_reply(vec![call("a", "bash", json!({"command": "touch one"}))]),
        tools_reply(vec![call("b", "bash", json!({"command": "touch two"}))]),
        FauxReply::text("ok"),
    ]);
    let yes = Arc::new(Yes(Default::default()));
    let agent = Agent::new(
        Arc::new(faux.clone()),
        novacode_tools::builtins(false),
        Store::create(&Paths::new(home.path()), work.path(), true),
        cfg(work.path(), home.path()),
        Arc::new(LocalEnv),
    )
    .with_policy(Policy::new(Permissions::default(), home.path().into(), None).with_mode(Mode::Ask))
    .with_approver(yes.clone());
    drive(agent, &["make files"]).await;
    assert!(work.path().join("one").exists() && work.path().join("two").exists());
    assert_eq!(
        *yes.0.lock().unwrap(),
        vec!["Bash(touch one:*)".to_string(), "Bash(touch two:*)".to_string()]
    );
}

/// L7.16: a hook's ask reaches the approver in auto mode; a hook's allow skips ask mode.
#[tokio::test]
async fn hook_ask_and_allow_reach_the_policy() {
    use novacode_core::hooks::{Gate, Handler, Hooks};
    use novacode_core::policy::{HookDecision, Mode, Permissions, Policy};
    struct Decide(Mode);
    impl Handler for Decide {
        fn before_tool<'a>(
            &'a self,
            _: &'a str,
            _: &'a serde_json::Value,
        ) -> futures_util::future::BoxFuture<'a, Gate> {
            let d = match self.0 {
                Mode::Auto => HookDecision::Ask("plugin asks".into()),
                _ => HookDecision::Allow,
            };
            Box::pin(async move { Gate::Decide(d, None) })
        }
    }
    for mode in [Mode::Auto, Mode::Ask] {
        let work = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let faux = Faux::new([
            tools_reply(vec![call("a", "bash", json!({"command": "touch one"}))]),
            FauxReply::text("ok"),
        ]);
        let yes = Arc::new(Yes(Default::default()));
        let agent = Agent::new(
            Arc::new(faux.clone()),
            novacode_tools::builtins(false),
            Store::create(&Paths::new(home.path()), work.path(), true),
            cfg(work.path(), home.path()),
            Arc::new(LocalEnv),
        )
        .with_policy(Policy::new(Permissions::default(), home.path().into(), None).with_mode(mode))
        .with_hooks(Arc::new(Hooks {
            handlers: vec![Arc::new(Decide(mode))],
        }))
        .with_approver(yes.clone());
        drive(agent, &["make a file"]).await;
        assert!(work.path().join("one").exists());
        let asked = yes.0.lock().unwrap().len();
        assert_eq!(asked, usize::from(mode == Mode::Auto), "{mode:?}");
    }
}

#[tokio::test]
async fn rejected_schema_keyword_is_stripped_and_remembered() {
    let work = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let faux = Faux::new([
        FauxReply::Fail(
            ErrorKind::InvalidRequest,
            "Invalid schema for function 'read': In context=('properties', 'offset'), 'minimum' is not permitted."
                .into(),
        ),
        FauxReply::text("ok"),
        FauxReply::text("again"),
    ]);
    let store = Store::create(&Paths::new(home.path()), work.path(), true);
    let quirks = Arc::new(novacode_ai::schema::Quirks::load(home.path()));
    let agent = Agent::new(
        Arc::new(faux.clone()),
        novacode_tools::builtins(false),
        store,
        cfg(work.path(), home.path()),
        Arc::new(LocalEnv),
    )
    .with_quirks(quirks);
    let (agent, events) = drive(agent, &["hi", "more"]).await;
    let reqs = faux.requests.lock().unwrap();
    let has_min = |r: &Request| r.tools.iter().any(|t| t.schema.to_string().contains("\"minimum\""));
    assert!(has_min(&reqs[0]), "builtin schemas carry `minimum` (schemars integers)");
    assert!(
        !has_min(&reqs[1]) && !has_min(&reqs[2]),
        "stripped on retry and for later turns"
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::Retry { delay_ms: 0, .. }))
    );
    // The failed attempt is hidden from the model.
    assert!(
        !reqs[1]
            .messages
            .iter()
            .any(|m| matches!(m, Message::Assistant(a) if a.error_message.is_some()))
    );
    drop(agent);
    let saved = std::fs::read_to_string(home.path().join("cache/schema-quirks.json")).unwrap();
    assert!(saved.contains("faux/m") && saved.contains("minimum"), "{saved}");
}

/// M6 check: a context file added mid-session reaches the model as an appended
/// delta; the system prompt and every earlier message keep their bytes (L4.7, L1.7).
#[tokio::test]
async fn prompt_changes_are_appended_deltas() {
    let work = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let faux = Faux::new([FauxReply::text("one"), FauxReply::text("two")]);
    let paths = Paths::new(home.path());
    let store = Store::create(&paths, work.path(), false);
    let agent = Agent::new(
        Arc::new(faux.clone()),
        novacode_tools::builtins(false),
        store,
        cfg(work.path(), home.path()),
        Arc::new(LocalEnv),
    );
    let (h, task) = agent::spawn(agent);
    let mut rx = h.subscribe();
    h.prompt("first");
    while !matches!(rx.recv().await.unwrap(), AgentEvent::AgentEnd { .. }) {}
    std::fs::write(work.path().join("AGENTS.md"), "Always answer in French.").unwrap();
    h.prompt("second");
    while !matches!(rx.recv().await.unwrap(), AgentEvent::AgentEnd { .. }) {}
    h.shutdown();
    let agent = task.await.unwrap();

    let reqs = faux.requests.lock().unwrap().clone();
    assert_eq!(reqs.len(), 2);
    let sys = |r: &Request| build_body(r, None, "anthropic")["system"].clone();
    assert_eq!(sys(&reqs[0]), sys(&reqs[1]), "the prompt itself never changes");
    let (a, b) = (wire_messages(&reqs[0]), wire_messages(&reqs[1]));
    assert!(b[..a.len()] == a[..], "request is not a prefix of the next");
    let tail = b[a.len()..].join("");
    assert!(
        tail.contains("Updated system prompt section \\\"project_context\\\"")
            && tail.contains("Always answer in French."),
        "{tail}"
    );
    // Nothing changed since: a third look appends nothing.
    assert_eq!(
        agent
            .store
            .sections()
            .iter()
            .filter(|(n, _)| n == "project_context")
            .count(),
        1
    );
}

/// M6 check: a reply whose cache read falls short of the previous prompt is
/// reported once, with its cause, in the transcript and in logs/cache-misses.jsonl.
#[tokio::test]
async fn cache_miss_is_named_and_logged() {
    let work = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let reply = |read, write| FauxReply::Message {
        blocks: vec![Block::text("ok")],
        stop: StopReason::Stop,
        usage: novacode_types::Usage {
            input: 10,
            cache_read: read,
            cache_write: write,
            ..Default::default()
        },
    };
    // Turn 2 reads the whole cached prompt; turn 3 reads nothing.
    let faux = Faux::new([reply(0, 20_000), reply(20_010, 50), reply(0, 20_100)]);
    let store = Store::create(&Paths::new(home.path()), work.path(), false);
    let agent = Agent::new(
        Arc::new(faux),
        vec![],
        store,
        cfg(work.path(), home.path()),
        Arc::new(LocalEnv),
    );
    let (_agent, events) = drive(agent, &["one", "two", "three"]).await;
    let notices: Vec<&String> = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::Stream(novacode_types::StreamEvent::Notice { text: n }) => Some(n),
            _ => None,
        })
        .collect();
    assert_eq!(notices.len(), 1, "{notices:?}");
    assert!(
        notices[0].starts_with("cache miss: 20060 prompt tokens")
            && notices[0].contains("(evicted by the provider: prefix unchanged"),
        "{}",
        notices[0]
    );
    let log = std::fs::read_to_string(home.path().join("logs/cache-misses.jsonl")).unwrap();
    assert!(log.contains("\"lost\":20060"), "{log}");
}

/// L11.11 check: a session with a known slow tool writes model, tool and task
/// records whose timings, tokens and cache hits add up.
#[tokio::test]
async fn metrics_record_calls_tools_and_tasks() {
    use novacode_core::metrics::{Record, load, path};
    let work = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let usage = |read, write| novacode_types::Usage {
        input: 100,
        output: 10,
        cache_read: read,
        cache_write: write,
        ..Default::default()
    };
    let faux = Faux::new([
        FauxReply::Message {
            blocks: vec![call("c1", "bash", json!({"command": "sleep 0.3; echo done"}))],
            stop: StopReason::ToolUse,
            usage: usage(0, 900),
        },
        FauxReply::Message {
            blocks: vec![Block::text("finished")],
            stop: StopReason::Stop,
            usage: usage(900, 50),
        },
    ]);
    let store = Store::create(&Paths::new(home.path()), work.path(), false);
    let id = store.id().to_string();
    let mut c = cfg(work.path(), home.path());
    c.metrics = true;
    let agent = Agent::new(
        Arc::new(faux),
        novacode_tools::builtins(false),
        store,
        c,
        Arc::new(LocalEnv),
    );
    let (_agent, _) = drive(agent, &["run the slow thing"]).await;
    let file = path(home.path(), &id);
    let mut recs = Vec::new();
    for _ in 0..100 {
        recs = load(&file);
        if recs.iter().any(|r| matches!(r, Record::Task { .. })) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(matches!(&recs[0], Record::Session { session, .. } if *session == id));
    let models: Vec<_> = recs.iter().filter(|r| matches!(r, Record::Model { .. })).collect();
    assert_eq!(models.len(), 2);
    let Record::Model {
        hit_pct,
        ctx_tokens,
        turn,
        ..
    } = models[1]
    else {
        panic!()
    };
    assert_eq!((*hit_pct, *ctx_tokens, *turn), (85.7, 1060, 2));
    let tool = recs.iter().find(|r| matches!(r, Record::Tool { .. })).unwrap();
    let Record::Tool {
        name,
        duration_ms,
        out_preview,
        args_preview,
        is_error,
        ..
    } = tool
    else {
        panic!()
    };
    assert_eq!((name.as_str(), *is_error), ("bash", false));
    assert!(*duration_ms >= 300, "{duration_ms}");
    assert!(
        args_preview.contains("sleep 0.3") && out_preview.contains("done"),
        "{tool:?}"
    );
    let task = recs.last().unwrap();
    let Record::Task {
        wall_ms,
        tools_ms,
        turns,
        tool_calls,
        usage,
        prompt_preview,
        stop,
        ..
    } = task
    else {
        panic!("{task:?}")
    };
    assert!(*tools_ms >= 300 && wall_ms >= tools_ms, "{task:?}");
    assert_eq!((*turns, *tool_calls, usage.cache_read, usage.input), (2, 1, 900, 200));
    assert_eq!((prompt_preview.as_str(), stop.as_str()), ("run the slow thing", "stop"));
    let turns: Vec<_> = recs.iter().filter(|r| matches!(r, Record::Turn { .. })).collect();
    assert_eq!(turns.len(), 2, "{recs:?}");
    let Record::Turn {
        turn,
        wall_ms,
        model_ms,
        preflight_ms,
        tools_ms,
        overhead_ms,
        tool_calls,
        usage,
        ..
    } = turns[0]
    else {
        panic!()
    };
    assert_eq!((*turn, *tool_calls, usage.cache_write), (1, 1, 900));
    assert!(*tools_ms >= 300, "{:?}", turns[0]);
    assert_eq!(
        *wall_ms,
        model_ms + preflight_ms + tools_ms + overhead_ms,
        "{:?}",
        turns[0]
    );
    assert!(matches!(
        turns[1],
        Record::Turn {
            turn: 2,
            tool_calls: 0,
            ..
        }
    ));
    assert!(matches!(task, Record::Task { idle_ms: None, .. }));
}

fn notes_reply(n: usize) -> FauxReply {
    FauxReply::Message {
        blocks: vec![call(
            &format!("n{n}"),
            "record_notes",
            json!({"notes": [{"content": format!("completed: step {n}"), "type": "change", "relevance": "high", "sources": [1]}]}),
        )],
        stop: StopReason::ToolUse,
        usage: Usage::default(),
    }
}

/// M7 check: a long session compacts with no LLM call for the summary. The
/// observer runs in the background as the session grows, catches up to the cut,
/// and the next request carries the rendered notes plus the kept tail.
#[tokio::test]
async fn long_session_compacts_from_notes() {
    use novacode_core::memory::{Settings, Worker};
    use novacode_core::session::EntryKind;
    let work = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let big = "lorem ipsum ".repeat(300); // ~1k tokens
    let main = Faux::new((0..8).map(|i| FauxReply::text(&format!("answer {i} {big}"))));
    let observer = Faux::new((0..20).map(notes_reply));
    let (main_reqs, obs_reqs) = (main.requests.clone(), observer.requests.clone());
    let store = Store::create(&Paths::new(home.path()), work.path(), false);
    let agent = Agent::new(
        Arc::new(main),
        vec![],
        store,
        cfg(work.path(), home.path()),
        Arc::new(LocalEnv),
    )
    .with_memory(
        Worker {
            adapter: Arc::new(observer),
            provider: "faux".into(),
            model: "small".into(),
        },
        Settings {
            observe_after: 1500,
            keep_recent: 2500,
            compact_at: 7000,
            chunk_tokens: 3000,
            reflect_after: u64::MAX,
            pool_target: u64::MAX,
        },
    );
    let prompts: Vec<String> = (0..8).map(|i| format!("task {i} {big}")).collect();
    let refs: Vec<&str> = prompts.iter().map(String::as_str).collect();
    let (agent, events) = drive(agent, &refs).await;
    let entries = agent.store.entries();
    let compactions: Vec<_> = entries
        .iter()
        .filter(|e| matches!(e.kind, EntryKind::Compaction { .. }))
        .collect();
    assert!(!compactions.is_empty(), "no compaction");
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::Compacted { fallback: false, .. }))
    );
    // Every main request is a real prompt: no summary call was made.
    assert_eq!(main_reqs.lock().unwrap().len(), 8);
    // Observer requests carry only the observer prompt and one chunk.
    for r in obs_reqs.lock().unwrap().iter() {
        assert_eq!(r.model, "small");
        assert_eq!(r.tools[0].name, "record_notes");
    }
    // The request after the first compaction: full prompt, then the summary in
    // front of the first kept user message, then the kept tail.
    let EntryKind::Compaction { first_kept, .. } = &compactions[0].kind else {
        panic!()
    };
    let after = agent.store.projection();
    assert!(matches!(&after[0], Message::System { .. }));
    let Message::User { content } = &after[1] else {
        panic!("{:?}", after[1])
    };
    let summary = format!("{:?}", content[0]);
    assert!(
        summary.contains("compaction-summary") && summary.contains("completed: step 0"),
        "{summary}"
    );
    assert!(
        !format!("{after:?}").contains("task 0 "),
        "the first prompt is compacted away"
    );
    assert!(entries.iter().any(|e| &e.id == first_kept));
    let total: u64 = after.iter().map(novacode_types::estimate::message).sum();
    assert!(total < 7000, "{total} tokens after compaction");
}

/// One prompt that runs many tool calls (the 16-hour EEC-Learning run): notes are
/// taken and the context compacts at `compact_at` during the run, not only after it.
#[tokio::test]
async fn long_run_compacts_between_model_calls() {
    use novacode_core::memory::{Settings, Worker};
    use novacode_core::session::EntryKind;
    let work = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let bash = |i: usize| {
        let cmd = format!("echo out{i}; head -c 4000 /dev/zero | tr '\\0' x");
        // No usage: the context is estimated from the messages themselves.
        FauxReply::Message {
            blocks: vec![call(&format!("b{i}"), "bash", json!({ "command": cmd }))],
            stop: StopReason::ToolUse,
            usage: Usage::default(),
        }
    };
    let mut replies: Vec<FauxReply> = (0..14).map(bash).collect();
    replies.push(FauxReply::text("done"));
    let main = Faux::new(replies);
    let main_reqs = main.requests.clone();
    let store = Store::create(&Paths::new(home.path()), work.path(), false);
    let agent = Agent::new(
        Arc::new(main),
        novacode_tools::builtins(false),
        store,
        cfg(work.path(), home.path()),
        Arc::new(LocalEnv),
    )
    .with_memory(
        Worker {
            adapter: Arc::new(Faux::new((0..40).map(notes_reply))),
            provider: "faux".into(),
            model: "small".into(),
        },
        Settings {
            observe_after: 2000,
            keep_recent: 2500,
            compact_at: 7000,
            chunk_tokens: 3000,
            reflect_after: u64::MAX,
            pool_target: u64::MAX,
        },
    );
    let (agent, _) = drive(agent, &["refine it until it works"]).await;
    let entries = agent.store.entries();
    let last_compaction = entries
        .iter()
        .rposition(|e| matches!(e.kind, EntryKind::Compaction { .. }))
        .expect("no compaction during the run");
    // The run went on after compacting: tool calls came after the compaction.
    assert!(
        entries[last_compaction..].iter().any(|e| matches!(
            &e.kind,
            EntryKind::Message {
                message: Message::ToolResult { .. }
            }
        )),
        "compacted only after the run"
    );
    // Every model call stayed near the threshold, never the whole run's output.
    for r in main_reqs.lock().unwrap().iter() {
        let t: u64 = r.messages.iter().map(novacode_types::estimate::message).sum();
        assert!(t < 12_000, "{t} tokens sent in one call");
    }
}

/// When the observer can't run, the session model summarises the gap instead.
#[tokio::test]
async fn compaction_without_notes_falls_back() {
    use novacode_core::memory::{Settings, Worker};
    let work = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let big = "lorem ipsum ".repeat(300);
    // Real usage that grows with the prompt: after compaction the kept reply's usage is stale.
    let main = Faux::new((0..8).map(|i| FauxReply::Message {
        blocks: vec![Block::text(format!("answer {i} {big}"))],
        stop: StopReason::Stop,
        usage: Usage {
            input: 1500 * (i + 1),
            ..Default::default()
        },
    }));
    let observer = Faux::new((0..20).map(|_| FauxReply::Fail(ErrorKind::Auth, "no login".into())));
    let store = Store::create(&Paths::new(home.path()), work.path(), false);
    let agent = Agent::new(
        Arc::new(main),
        vec![],
        store,
        cfg(work.path(), home.path()),
        Arc::new(LocalEnv),
    )
    .with_memory(
        Worker {
            adapter: Arc::new(observer),
            provider: "faux".into(),
            model: "small".into(),
        },
        Settings {
            observe_after: 1_000_000,
            keep_recent: 2500,
            compact_at: 6000,
            chunk_tokens: 3000,
            reflect_after: u64::MAX,
            pool_target: u64::MAX,
        },
    );
    let prompts: Vec<String> = (0..5).map(|i| format!("task {i} {big}")).collect();
    let refs: Vec<&str> = prompts.iter().map(String::as_str).collect();
    let (agent, events) = drive(agent, &refs).await;
    assert!(events.iter().any(|e| matches!(
        e,
        AgentEvent::Compacted {
            fallback: false,
            notes: 0,
            ..
        }
    )));
    // The context after compaction is estimated, not the last reply's pre-compaction usage.
    for e in &events {
        if let AgentEvent::Compacted {
            tokens_before,
            tokens_after,
            ..
        } = e
        {
            assert!(tokens_after < tokens_before, "{tokens_after} >= {tokens_before}");
        }
    }
    let p = format!("{:?}", agent.store.projection());
    assert!(p.contains("Summary of the earlier conversation"), "{p}");
}

/// L4.5 revision: with the observer off (`memory.enabled: false`) a long session
/// still compacts, summarising with the session model; the observer is never called.
#[tokio::test]
async fn compaction_without_the_observer() {
    use novacode_core::memory::{Settings, Worker};
    let work = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let big = "lorem ipsum ".repeat(300);
    let main = Faux::new((0..8).map(|i| FauxReply::text(&format!("answer {i} {big}"))));
    let observer = Faux::new((0..20).map(notes_reply));
    let agent = Agent::new(
        Arc::new(main),
        vec![],
        Store::create(&Paths::new(home.path()), work.path(), false),
        cfg(work.path(), home.path()),
        Arc::new(LocalEnv),
    )
    .with_memory(
        Worker {
            adapter: Arc::new(observer.clone()),
            provider: "faux".into(),
            model: "small".into(),
        },
        Settings {
            observe_after: 1000,
            keep_recent: 2500,
            compact_at: 6000,
            chunk_tokens: 3000,
            reflect_after: u64::MAX,
            pool_target: u64::MAX,
        },
    )
    .without_observer();
    let prompts: Vec<String> = (0..5).map(|i| format!("task {i} {big}")).collect();
    let refs: Vec<&str> = prompts.iter().map(String::as_str).collect();
    let (agent, events) = drive(agent, &refs).await;
    assert!(events.iter().any(|e| matches!(e, AgentEvent::Compacted { .. })));
    assert!(observer.requests.lock().unwrap().is_empty(), "the observer never ran");
    let p = format!("{:?}", agent.store.projection());
    assert!(p.contains("Summary of the earlier conversation"), "{p}");
}

/// A model switch moves background calls too: the observer to the switch's helper,
/// and (observer off) compaction summaries to the new session model.
#[tokio::test]
async fn background_models_follow_a_model_switch() {
    use novacode_core::agent::Switch;
    use novacode_core::memory::{Settings, Worker};
    let big = "lorem ipsum ".repeat(300);
    let settings = Settings {
        observe_after: 1000,
        keep_recent: 2500,
        compact_at: 6000,
        chunk_tokens: 3000,
        reflect_after: u64::MAX,
        pool_target: u64::MAX,
    };
    let prompts: Vec<String> = (0..5).map(|i| format!("task {i} {big}")).collect();
    let refs: Vec<&str> = prompts.iter().map(String::as_str).collect();
    for observe in [true, false] {
        let work = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let (old_main, old_small) = (Faux::new([]), Faux::new([]));
        let new_main = Faux::new((0..20).map(|i| FauxReply::text(&format!("answer {i} {big}"))));
        let new_small = Faux::new((0..20).map(notes_reply));
        let worker = |f: &Faux, model: &str| Worker {
            adapter: Arc::new(f.clone()),
            provider: "faux".into(),
            model: model.into(),
        };
        let mut agent = Agent::new(
            Arc::new(old_main.clone()),
            vec![],
            Store::create(&Paths::new(home.path()), work.path(), false),
            cfg(work.path(), home.path()),
            Arc::new(LocalEnv),
        )
        .with_memory(worker(&old_small, "old-small"), settings.clone());
        if !observe {
            agent = agent.without_observer();
        }
        let sw = Switch {
            provider: "faux".into(),
            model: "new".into(),
            adapter: Arc::new(new_main.clone()),
            window_tokens: 200_000,
            max_tokens: 4096,
            images: false,
            reasoning: false,
            thinking: Default::default(),
            helper: Some(worker(&new_small, "new-small")),
        };
        let (_, events) = drive_with(agent, &refs, |h| h.set_model(sw)).await;
        assert!(
            events.iter().any(|e| matches!(e, AgentEvent::Compacted { .. })),
            "observe {observe}"
        );
        assert!(old_main.requests.lock().unwrap().is_empty(), "observe {observe}");
        assert!(old_small.requests.lock().unwrap().is_empty(), "observe {observe}");
        assert_eq!(!new_small.requests.lock().unwrap().is_empty(), observe);
    }
}

/// One prompt followed by many tool calls (the novabrowser session): `/compact`
/// cuts inside the turn instead of keeping all of it, and the prompt stays word
/// for word in the summary, which gets a user message of its own.
#[tokio::test]
async fn compact_splits_one_long_turn_and_keeps_its_prompt() {
    use novacode_core::memory::{Settings, Worker};
    let work = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    // Each call prints ~1k tokens.
    let bash = |i: usize| {
        let cmd = format!("echo out{i}; head -c 4000 /dev/zero | tr '\\0' x");
        tools_reply(vec![call(&format!("b{i}"), "bash", json!({ "command": cmd }))])
    };
    let mut replies: Vec<FauxReply> = (0..10).map(bash).collect();
    replies.push(FauxReply::text("done"));
    let observer = Faux::new((0..20).map(notes_reply));
    let store = Store::create(&Paths::new(home.path()), work.path(), false);
    let agent = Agent::new(
        Arc::new(Faux::new(replies)),
        novacode_tools::builtins(false),
        store,
        cfg(work.path(), home.path()),
        Arc::new(LocalEnv),
    )
    .with_memory(
        Worker {
            adapter: Arc::new(observer),
            provider: "faux".into(),
            model: "small".into(),
        },
        Settings {
            observe_after: 1_000_000,
            keep_recent: 2500,
            compact_at: u64::MAX,
            chunk_tokens: 3000,
            reflect_after: u64::MAX,
            pool_target: u64::MAX,
        },
    );
    let (mut agent, _) = drive(agent, &["add a --profile flag"]).await;
    assert!(agent.compact("you asked for it").await, "nothing was compacted");
    let after = agent.store.projection();
    let p = format!("{after:?}");
    assert!(!p.contains("out0"), "the turn's early tool output is compacted away");
    assert!(p.contains("out9"), "the recent tool output is kept");
    let Message::User { content } = &after[1] else {
        panic!("{:?}", after[1])
    };
    let summary = format!("{content:?}");
    assert!(
        summary.contains("compaction-summary")
            && summary.contains("Request in progress")
            && summary.contains("add a --profile flag"),
        "{summary}"
    );
    assert_eq!(content.len(), 1, "the summary has a user message of its own");
    assert!(matches!(&after[2], Message::Assistant(_)), "{:?}", after[2]);
}

/// The reflector keeps only supports that name active notes; the dropper's
/// proposals are capped by code to what brings the pool back to its target.
#[tokio::test]
async fn reflector_and_dropper_respect_their_budgets() {
    use novacode_core::memory::{Note, Relevance, Worker, drop_notes, note_id, pool_tokens, reflect};
    let note = |c: &str, r, ts| Note {
        id: note_id(c),
        content: c.into(),
        relevance: r,
        kind: "change".into(),
        sources: vec!["e".into()],
        ts_ms: ts,
    };
    let notes = vec![
        note("User prefers tabs", Relevance::Critical, 1),
        note("ran cargo test, passed", Relevance::Low, 2),
        note("read src/lib.rs", Relevance::Low, 3),
        note("chose sqlx over diesel", Relevance::High, 4),
    ];
    let ids: Vec<String> = notes.iter().map(|n| n.id.clone()).collect();
    let tool = |name: &str, args: Value| FauxReply::Message {
        blocks: vec![call("t", name, args)],
        stop: StopReason::ToolUse,
        usage: Usage::default(),
    };
    let faux = Faux::new([
        tool(
            "record_reflections",
            json!({"reflections": [
                {"content": "User prefers tabs and chose sqlx", "supports": [ids[0], ids[3], "bogus00000000"]},
                {"content": "no support", "supports": ["bogus00000000"]},
            ]}),
        ),
        tool("drop_notes", json!({"ids": [ids[2], ids[1], ids[0]]})),
    ]);
    let w = Worker {
        adapter: Arc::new(faux),
        provider: "faux".into(),
        model: "small".into(),
    };
    let (refl, _) = reflect(&w, &[], &notes, "s").await.unwrap();
    assert_eq!(refl.len(), 1);
    assert_eq!(refl[0].supports, vec![ids[0].clone(), ids[3].clone()]);
    // Target: one note's worth below the pool, so at most one drop is allowed.
    let target = pool_tokens(&notes) - pool_tokens(&notes) / notes.len() as u64;
    let (dropped, _) = drop_notes(&w, &refl, &notes, target, "s").await.unwrap();
    // Cited-by-a-reflection goes first: the tabs note, even though it's critical,
    // because the reflection keeps its meaning; the rest is capped away.
    assert_eq!(dropped, vec![ids[0].clone()]);
}

/// M7 check: each prompt gets a checkpoint before any tool runs; rewinding to a
/// prompt restores its files, bash changes and created files included.
#[tokio::test]
async fn rewind_restores_files_of_a_prompt() {
    use novacode_core::checkpoint;
    let work = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let w = work.path();
    let git = |a: &[&str]| {
        assert!(
            std::process::Command::new("git")
                .arg("-C")
                .arg(w)
                .args(a)
                .status()
                .unwrap()
                .success()
        )
    };
    git(&["init", "-q"]);
    std::fs::write(w.join("a.txt"), "one\n").unwrap();
    let bash = |id: &str, cmd: &str| tools_reply(vec![call(id, "bash", json!({ "command": cmd }))]);
    let faux = Faux::new([
        bash("b1", "echo two > a.txt && echo new > b.txt"),
        FauxReply::text("done"),
        bash("b2", "echo three > a.txt && rm b.txt"),
        FauxReply::text("done"),
    ]);
    let store = Store::create(&Paths::new(home.path()), w, false);
    let repo = checkpoint::resolve(w, home.path()).unwrap();
    let agent = Agent::new(
        Arc::new(faux),
        novacode_tools::builtins(false),
        store,
        cfg(w, home.path()),
        Arc::new(LocalEnv),
    )
    .with_checkpoints(repo.clone());
    let (agent, _) = drive(agent, &["first", "second"]).await;
    assert_eq!(std::fs::read_to_string(w.join("a.txt")).unwrap(), "three\n");
    let prompts = agent.store.prompts();
    let (first, second) = (&prompts[0].id, &prompts[1].id);
    // Back to before "second": the first prompt's result.
    let r = checkpoint::rewind(&agent.store, &repo, second).unwrap();
    assert_eq!(std::fs::read_to_string(w.join("a.txt")).unwrap(), "two\n");
    assert!(w.join("b.txt").exists());
    // Back to before "first": the original, created file gone.
    checkpoint::rewind(&agent.store, &repo, first).unwrap();
    assert_eq!(std::fs::read_to_string(w.join("a.txt")).unwrap(), "one\n");
    assert!(!w.join("b.txt").exists());
    // Undo of the first rewind returns to the end state.
    checkpoint::undo(&agent.store, &repo, &r.undo).unwrap();
    assert_eq!(std::fs::read_to_string(w.join("a.txt")).unwrap(), "three\n");
}

/// L5.17: a deferred tool is refused until found, then runs through `call`
/// (non-native model) with the real tool name in the pipeline.
#[tokio::test]
async fn deferred_tools_run_through_call() {
    let work = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    std::fs::write(work.path().join("notes.txt"), "deferred hello").unwrap();
    let faux = Faux::new([
        tools_reply(vec![call("a", "read", json!({"path": "notes.txt"}))]),
        tools_reply(vec![call(
            "b",
            "call",
            json!({"name": "read", "arguments": {"path": "notes.txt"}}),
        )]),
        FauxReply::text("done"),
    ]);
    let all = novacode_tools::builtins(false);
    let (listed, deferred): (Vec<_>, Vec<_>) = all.into_iter().partition(|t| t.name() != "read");
    let mut listed = listed;
    listed.push(Arc::new(novacode_tools::toolsearch::Call));
    let store = Store::create(&Paths::new(home.path()), work.path(), true);
    let agent = Agent::new(
        Arc::new(faux.clone()),
        listed,
        store,
        cfg(work.path(), home.path()),
        Arc::new(LocalEnv),
    )
    .with_deferred(deferred);
    let (agent, _) = drive(agent, &["read it"]).await;
    let results: Vec<String> = agent
        .store
        .projection()
        .iter()
        .filter(|m| matches!(m, Message::ToolResult { .. }))
        .map(result_text)
        .collect();
    assert!(results[0].contains("find it with tool_search"), "{results:?}");
    assert!(results[1].contains("deferred hello"), "{results:?}");
    // The request never declared `read`.
    let req = faux.requests.lock().unwrap().clone();
    assert!(req[0].tools.iter().all(|t| t.name != "read"));
}

/// L7.5: codemode chains calls through the pipeline (permissions apply inside
/// scripts), returns only what it prints, and a transaction rolls back on error.
#[tokio::test]
async fn codemode_chains_through_the_pipeline() {
    use novacode_core::policy::{Permissions, Policy};
    let work = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    std::fs::write(work.path().join("a.txt"), "alpha").unwrap();
    std::fs::write(work.path().join("b.txt"), "beta").unwrap();
    let script = "const [a, b] = await Promise.all([tools.read({path: 'a.txt'}), tools.read({path: 'b.txt'})]);\n\
                  let denied = '';\n\
                  try { await tools.bash({command: 'git push origin main'}); } catch (e) { denied = e.message; }\n\
                  text(a.includes('alpha') && b.includes('beta') ? 'both read' : 'missing');\n\
                  return denied.includes('git push') ? 'push refused' : 'push ran: ' + denied;";
    let tx = "await tools.write({path: 'a.txt', content: 'changed'});\nawait tools.write({path: 'new.txt', content: 'x'});\nthrow new Error('stop');";
    let faux = Faux::new([
        tools_reply(vec![call("a", "codemode", json!({ "code": script }))]),
        tools_reply(vec![call("b", "codemode", json!({ "code": tx, "transaction": true }))]),
        FauxReply::text("done"),
    ]);
    let mut tools = novacode_tools::builtins(false);
    tools.push(Arc::new(novacode_core::codemode::Codemode::default()));
    let perms: Permissions = serde_json::from_value(json!({ "deny": ["Bash(git push:*)"] })).unwrap();
    let agent = Agent::new(
        Arc::new(faux.clone()),
        tools,
        Store::create(&Paths::new(home.path()), work.path(), true),
        cfg(work.path(), home.path()),
        Arc::new(LocalEnv),
    )
    .with_policy(Policy::new(perms, home.path().into(), None));
    let (agent, _) = drive(agent, &["go"]).await;
    let results: Vec<String> = agent
        .store
        .projection()
        .iter()
        .filter(|m| matches!(m, Message::ToolResult { .. }))
        .map(result_text)
        .collect();
    assert!(
        results[0].starts_with(
            "both read\npush refused\n(the script stopped calling tools after a refused call)\n[3 calls · "
        ),
        "{}",
        results[0]
    );
    assert!(
        results[1].contains("Script failed: Error: stop") && results[1].contains("2 file(s) restored"),
        "{}",
        results[1]
    );
    assert_eq!(std::fs::read_to_string(work.path().join("a.txt")).unwrap(), "alpha");
    assert!(!work.path().join("new.txt").exists());
}

/// Read-only mode holds inside codemode scripts and through `call` too (revision L6.1).
#[tokio::test]
async fn read_only_holds_for_nested_calls() {
    use novacode_core::policy::{Mode, Permissions, Policy};
    let work = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let script = "let out = [];\n\
                  for (const c of [() => tools.process({action: 'start', command: 'touch by-script'}), () => tools.write({path: 'w.txt', content: 'x'})]) {\n\
                    try { await c(); out.push('ran'); } catch (e) { out.push('refused'); }\n\
                  }\n\
                  return out.join(',');";
    let faux = Faux::new([
        tools_reply(vec![
            call("a", "codemode", json!({ "code": script })),
            call(
                "b",
                "call",
                json!({ "name": "write", "arguments": {"path": "c.txt", "content": "x"} }),
            ),
        ]),
        FauxReply::text("done"),
    ]);
    // `write` is deferred, so `call` reaches it; codemode reaches it as well.
    let (mut tools, deferred): (Vec<_>, Vec<_>) = novacode_tools::builtins(false)
        .into_iter()
        .partition(|t| t.name() != "write");
    tools.push(Arc::new(novacode_tools::process::ProcessTool));
    tools.push(Arc::new(novacode_core::codemode::Codemode::default()));
    tools.push(Arc::new(novacode_tools::toolsearch::Call));
    let agent = Agent::new(
        Arc::new(faux.clone()),
        tools,
        Store::create(&Paths::new(home.path()), work.path(), true),
        cfg(work.path(), home.path()),
        Arc::new(LocalEnv),
    )
    .with_deferred(deferred)
    .with_processes(novacode_tools::process::ProcessManager::new(
        &home.path().join("processes"),
        "s",
    ))
    .with_policy(Policy::new(Permissions::default(), home.path().into(), None).with_mode(Mode::ReadOnly));
    let (agent, _) = drive(agent, &["go"]).await;
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    let results: Vec<String> = agent
        .store
        .projection()
        .iter()
        .filter(|m| matches!(m, Message::ToolResult { .. }))
        .map(result_text)
        .collect();
    assert!(
        results[0].starts_with("refused,refused\n(the script stopped calling tools after a refused call)\n[2 calls · "),
        "{results:?}"
    );
    assert!(results[1].contains("can't change files"), "{results:?}");
    for f in ["by-script", "w.txt", "c.txt"] {
        assert!(!work.path().join(f).exists(), "{f}");
    }
}

/// The same lines of an unchanged file, still in the context, are not sent again;
/// a changed file or other lines are.
#[tokio::test]
async fn unchanged_reread_points_at_the_earlier_result() {
    let work = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    std::fs::write(work.path().join("a.txt"), "alpha\nbeta\n").unwrap();
    let read = |id: &str, extra: serde_json::Value| {
        let mut a = json!({ "path": "a.txt" });
        a.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
        tools_reply(vec![call(id, "read", a)])
    };
    let faux = Faux::new([
        read("r1", json!({})),
        read("r2", json!({})),
        read("r3", json!({ "offset": 2 })),
        tools_reply(vec![call(
            "w",
            "write",
            json!({ "path": "a.txt", "content": "gamma\n" }),
        )]),
        read("r4", json!({})),
        read("r5", json!({})),
        FauxReply::text("done"),
    ]);
    let agent = Agent::new(
        Arc::new(faux),
        novacode_tools::builtins(false),
        Store::create(&Paths::new(home.path()), work.path(), true),
        cfg(work.path(), home.path()),
        Arc::new(LocalEnv),
    );
    let (agent, _) = drive(agent, &["go"]).await;
    let out: Vec<String> = agent
        .store
        .projection()
        .iter()
        .filter(|m| matches!(m, Message::ToolResult { tool_name, .. } if tool_name == "read"))
        .map(result_text)
        .collect();
    let unchanged = |s: &str| s.contains("unchanged since your last read");
    assert!(out[0].contains("alpha") && unchanged(&out[1]), "{out:?}");
    assert!(
        out[2].contains("beta") && !unchanged(&out[2]),
        "other lines are sent: {out:?}"
    );
    assert!(out[3].contains("gamma"), "a changed file is sent: {out:?}");
    assert!(unchanged(&out[4]), "{out:?}");
}

/// Scripts get tool output whole (the novahome sessions' `JSON.parse` failures): a
/// 3000-line JSON file with a 5000-char line parses, and `tools.call` reaches the named tool.
#[tokio::test]
async fn codemode_gets_whole_tool_output() {
    let work = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let rows: Vec<String> = (0..3000).map(|i| format!("{{\"i\": {i}}}")).collect();
    let big = format!(
        "{{\"long\": \"{}\",\n\"rows\": [\n{}\n]}}",
        "x".repeat(5000),
        rows.join(",\n")
    );
    std::fs::write(work.path().join("d.json"), big).unwrap();
    let script = "const d = JSON.parse(await tools.read({path: 'd.json'}));\n\
                  const c = await tools.call({name: 'read', arguments: {path: 'd.json', offset: 2, limit: 1}});\n\
                  return [d.long.length, d.rows.length, c].join(' ');";
    let faux = Faux::new([
        tools_reply(vec![call("a", "codemode", json!({ "code": script }))]),
        FauxReply::text("done"),
    ]);
    let mut tools = novacode_tools::builtins(false);
    tools.push(Arc::new(novacode_core::codemode::Codemode::default()));
    let agent = Agent::new(
        Arc::new(faux),
        tools,
        Store::create(&Paths::new(home.path()), work.path(), true),
        cfg(work.path(), home.path()),
        Arc::new(LocalEnv),
    );
    let (agent, _) = drive(agent, &["go"]).await;
    let out: Vec<String> = agent
        .store
        .projection()
        .iter()
        .filter(|m| matches!(m, Message::ToolResult { .. }))
        .map(result_text)
        .collect();
    assert!(out[0].starts_with("5000 3000 \"rows\": [\n[2 calls · "), "{}", out[0]);
}

/// A tool result with an image, for codemode.
struct Snap;
impl novacode_tools::Tool for Snap {
    fn name(&self) -> &str {
        "snap"
    }
    fn description(&self) -> &str {
        "takes a screenshot"
    }
    fn schema(&self) -> Value {
        json!({ "type": "object", "properties": {} })
    }
    fn read_only(&self) -> bool {
        true
    }
    fn execute<'a>(
        &'a self,
        _args: Value,
        _ctx: &'a novacode_tools::ToolCtx,
    ) -> futures_util::future::BoxFuture<'a, novacode_tools::ToolOutput> {
        Box::pin(async {
            novacode_tools::ToolOutput {
                content: vec![
                    Block::text("shot"),
                    Block::Image {
                        mime_type: "image/png".into(),
                        data: "iVBORw0KGgo=".into(),
                    },
                ],
                is_error: false,
                details: None,
            }
        })
    }
}

/// L7.17: a script that catches a refusal can't call tools after it, and a
/// nested result's images reach the script as `{text, images}`.
#[tokio::test]
async fn codemode_stops_after_a_refusal_and_gets_images() {
    use novacode_core::policy::{Permissions, Policy};
    let work = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let refused = "try { await tools.bash({command: 'git push origin main'}); } catch (e) {}\n\
                   try { await tools.write({path: 'x.txt', content: 'x'}); return 'wrote'; } catch (e) { return e.message; }";
    let snap = "const r = await tools.snap({});\nimage(r.images[0]);\nreturn r.text;";
    let faux = Faux::new([
        tools_reply(vec![call("a", "codemode", json!({ "code": refused }))]),
        tools_reply(vec![call("b", "codemode", json!({ "code": snap }))]),
        FauxReply::text("done"),
    ]);
    let mut tools = novacode_tools::builtins(false);
    tools.push(Arc::new(Snap));
    tools.push(Arc::new(novacode_core::codemode::Codemode::default()));
    let perms: Permissions = serde_json::from_value(json!({ "deny": ["Bash(git push:*)"] })).unwrap();
    let agent = Agent::new(
        Arc::new(faux),
        tools,
        Store::create(&Paths::new(home.path()), work.path(), true),
        cfg(work.path(), home.path()),
        Arc::new(LocalEnv),
    )
    .with_policy(Policy::new(perms, home.path().into(), None));
    let (agent, _) = drive(agent, &["go"]).await;
    let results: Vec<Message> = agent
        .store
        .projection()
        .into_iter()
        .filter(|m| matches!(m, Message::ToolResult { .. }))
        .collect();
    let first = result_text(&results[0]);
    assert!(
        first.starts_with("write: not run: this script stopped calling tools after a refused call (Denied"),
        "{first}"
    );
    assert!(!work.path().join("x.txt").exists());
    let Message::ToolResult { content, .. } = &results[1] else {
        unreachable!()
    };
    assert!(
        matches!(&content[0], Block::Image { data, mime_type } if data == "iVBORw0KGgo=" && mime_type == "image/png"),
        "{content:?}"
    );
    assert!(result_text(&results[1]).starts_with("shot\n[1 call · "));
}

/// L7.17: the first refusal also stops the script's calls already in flight: a
/// parallel write waiting on its approval card is ended and never runs.
#[tokio::test]
async fn codemode_refusal_stops_calls_in_flight() {
    use novacode_core::policy::{Permissions, Policy};
    let work = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let script = "const r = await Promise.allSettled([tools.write({path: 'x.txt', content: 'x'}), tools.bash({command: 'git push origin main'})]);\n\
                  return r.map(x => x.status + ': ' + (x.reason ? x.reason.message : x.value)).join('\\n');";
    let faux = Faux::new([
        tools_reply(vec![call("a", "codemode", json!({ "code": script }))]),
        FauxReply::text("done"),
    ]);
    let mut tools = novacode_tools::builtins(false);
    tools.push(Arc::new(novacode_core::codemode::Codemode::default()));
    let perms: Permissions = serde_json::from_value(json!({ "deny": ["Bash(git push:*)"], "ask": ["Write"] })).unwrap();
    let asked = Arc::new(tokio::sync::Notify::new());
    let agent = Agent::new(
        Arc::new(faux),
        tools,
        Store::create(&Paths::new(home.path()), work.path(), true),
        cfg(work.path(), home.path()),
        Arc::new(LocalEnv),
    )
    .with_policy(Policy::new(perms, home.path().into(), None))
    .with_approver(Arc::new(Never(asked.clone())));
    let (agent, _) = tokio::time::timeout(std::time::Duration::from_secs(10), drive(agent, &["go"]))
        .await
        .expect("the open approval ends with the refusal");
    let results: Vec<Message> = agent
        .store
        .projection()
        .into_iter()
        .filter(|m| matches!(m, Message::ToolResult { .. }))
        .collect();
    let out = result_text(&results[0]);
    assert!(
        out.starts_with("rejected: write: Not run: aborted. (at line 1)\nrejected: bash: Denied"),
        "{out}"
    );
    assert!(!work.path().join("x.txt").exists());
    tokio::time::timeout(std::time::Duration::ZERO, asked.notified())
        .await
        .expect("the write's card was open when the refusal landed");
}

/// Extension context injection without cache misses: `remind` notes are saved and
/// re-sent unchanged; `before_request` text rides after the cached prefix of one
/// request only and is never stored.
#[tokio::test]
async fn extension_reminders_are_saved_and_tails_are_not() {
    let work = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let ext = home.path().join("extensions");
    std::fs::create_dir_all(&ext).unwrap();
    std::fs::write(
        ext.join("ctx.js"),
        "let n = 0;
         export default (nc) => {
           nc.on('before_request', ({ lastUser }) => `request ${++n} for: ${lastUser}`);
           nc.registerTool({ name: 'note', description: 'leave a note', execute: async () => { await nc.remind('remember: tabs'); return 'ok'; } });
         };",
    )
    .unwrap();
    let exts =
        novacode_core::extensions::Extensions::load(&[ext], work.path(), home.path(), &home.path().join("cache")).await;
    let faux = Faux::new([
        tools_reply(vec![call("c1", "note", json!({}))]),
        FauxReply::text("noted"),
        FauxReply::text("second"),
    ]);
    let store = Store::create(&Paths::new(home.path()), work.path(), false);
    let mut tools = novacode_tools::builtins(false);
    tools.extend(exts.tools());
    let agent = Agent::new(
        Arc::new(faux.clone()),
        tools,
        store,
        cfg(work.path(), home.path()),
        Arc::new(LocalEnv),
    )
    .with_extensions(exts);
    let (agent, _) = drive(agent, &["one", "two"]).await;
    let reqs = faux.requests.lock().unwrap().clone();
    assert_eq!(reqs.len(), 3);
    // Each request gets its own fresh tail, built from the latest user text.
    assert_eq!(reqs[0].tail, vec!["request 1 for: one".to_string()]);
    assert_eq!(reqs[2].tail, vec!["request 3 for: two".to_string()]);
    // The reminder is stored (hidden) and re-sent in later requests.
    let reminder = |r: &Request| {
        r.messages
            .iter()
            .any(|m| matches!(m, Message::User { content } if content.iter().any(|b| matches!(b, Block::Text { text, .. } if text.contains("remember: tabs")))))
    };
    assert!(!reminder(&reqs[0]) && reminder(&reqs[2]));
    // Stored history never holds a tail, and every request is a prefix of the next.
    let stored = std::fs::read_to_string(agent.store.path().unwrap()).unwrap();
    assert!(!stored.contains("request 1 for"), "tails are never saved");
    for w in reqs.windows(2) {
        let mut untailed = w[0].clone();
        untailed.tail.clear();
        let (a, b) = (wire_messages(&untailed), wire_messages(&w[1]));
        assert_eq!(
            &b[..a.len()],
            &a[..],
            "minus its tail, each request is a prefix of the next"
        );
    }
    // On the wire the tail comes after the last cache marker.
    let body = build_body(&reqs[2], None, "anthropic");
    let last = body["messages"].as_array().unwrap().last().unwrap()["content"]
        .as_array()
        .unwrap()
        .clone();
    assert!(last[last.len() - 2].get("cache_control").is_some());
    assert!(
        last.last().unwrap()["text"]
            .as_str()
            .unwrap()
            .contains("request 3 for: two")
    );
    assert!(last.last().unwrap().get("cache_control").is_none());
}

/// `/btw`: the side question rides on the last request's bytes, and nothing of it
/// is stored or seen by the next main request.
#[tokio::test]
async fn side_question_reuses_the_prefix_and_leaves_no_trace() {
    let work = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let faux = Faux::new([
        FauxReply::text("main answer"),
        FauxReply::text("side answer"),
        FauxReply::text("follow-up answer"),
        FauxReply::text("next main"),
    ]);
    let agent = Agent::new(
        Arc::new(faux.clone()),
        vec![],
        Store::create(&Paths::new(home.path()), work.path(), true),
        cfg(work.path(), home.path()),
        Arc::new(LocalEnv),
    );
    let (h, task) = agent::spawn(agent);
    let mut rx = h.subscribe();
    let until_end = |rx: &mut tokio::sync::broadcast::Receiver<AgentEvent>| {
        let mut rx = rx.resubscribe();
        async move { while !matches!(rx.recv().await.unwrap(), AgentEvent::Settled) {} }
    };
    let wait = until_end(&mut rx);
    h.prompt("main question");
    wait.await;
    assert_eq!(h.side_question("side?").await.unwrap(), "side answer");
    assert_eq!(h.side_question("and?").await.unwrap(), "follow-up answer");
    let wait = until_end(&mut rx);
    h.prompt("go on");
    wait.await;
    h.shutdown();
    let a = task.await.unwrap();
    let reqs = faux.requests.lock().unwrap();
    let first = wire_messages(&reqs[0]);
    assert_eq!(
        wire_messages(&reqs[1])[..first.len()],
        first[..],
        "the side request extends the main prefix"
    );
    assert_eq!(
        reqs[2].messages.len(),
        reqs[1].messages.len() + 2,
        "follow-up continues the side thread"
    );
    let main = format!("{:?}", reqs[3].messages);
    assert!(!main.contains("side?") && !main.contains("side answer"), "{main}");
    let stored = format!("{:?}", a.store.projection());
    assert!(!stored.contains("side answer"));
}

/// `/btw` runs read-only tools in a side loop; a write is refused and nothing changes.
#[tokio::test]
async fn side_question_runs_only_read_only_tools() {
    let work = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    std::fs::write(work.path().join("notes.txt"), "alpha\n").unwrap();
    let faux = Faux::new([
        FauxReply::text("main answer"),
        tools_reply(vec![
            call("s1", "read", json!({"path": "notes.txt"})),
            call("s2", "write", json!({"path": "notes.txt", "content": "gone"})),
        ]),
        FauxReply::text("it says alpha"),
    ]);
    let agent = Agent::new(
        Arc::new(faux.clone()),
        novacode_tools::builtins(false),
        Store::create(&Paths::new(home.path()), work.path(), true),
        cfg(work.path(), home.path()),
        Arc::new(LocalEnv),
    );
    let (h, task) = agent::spawn(agent);
    let mut rx = h.subscribe();
    h.prompt("main question");
    while !matches!(rx.recv().await.unwrap(), AgentEvent::Settled) {}
    assert_eq!(h.side_question("what's in notes?").await.unwrap(), "it says alpha");
    h.shutdown();
    task.await.unwrap();
    assert_eq!(
        std::fs::read_to_string(work.path().join("notes.txt")).unwrap(),
        "alpha\n"
    );
    let reqs = faux.requests.lock().unwrap();
    assert_eq!(reqs[2].tools, reqs[1].tools, "the tool list stays as sent");
    let results: Vec<_> = reqs[2]
        .messages
        .iter()
        .filter_map(|m| match m {
            Message::ToolResult { content, is_error, .. } => Some((format!("{content:?}"), *is_error)),
            _ => None,
        })
        .collect();
    assert_eq!(results.len(), 2);
    assert!(results[0].0.contains("alpha") && !results[0].1, "{results:?}");
    assert!(results[1].1, "write is refused: {results:?}");
}

#[tokio::test]
async fn esc_before_the_first_reply_takes_the_prompt_back() {
    let work = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let faux = Faux::new([
        FauxReply::Fail(ErrorKind::Aborted, "aborted".into()),
        FauxReply::text("ok"),
    ]);
    let store = Store::create(&Paths::new(home.path()), work.path(), true);
    let agent = Agent::new(
        Arc::new(faux.clone()),
        vec![],
        store,
        cfg(work.path(), home.path()),
        Arc::new(LocalEnv),
    );
    let (agent, events) = drive(agent, &["fix it", "again"]).await;
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::PromptTakenBack { text } if text == "fix it"))
    );
    // The taken-back prompt and the empty aborted reply are off the branch.
    let reqs = faux.requests.lock().unwrap();
    let users: Vec<_> = reqs[1]
        .messages
        .iter()
        .filter(|m| matches!(m, Message::User { .. }))
        .collect();
    assert_eq!(users.len(), 1, "{users:?}");
    assert!(!reqs[1].messages.iter().any(|m| matches!(m, Message::Assistant(_))));
    drop(agent);
}

/// L7.16 check: a skill the user activates (through the session handle) adds its
/// `PreToolUse` hook, which then refuses the model's bash call.
#[tokio::test]
async fn activated_skill_hooks_gate_tools() {
    let work = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let dir = home.path().join("skills/guard");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("SKILL.md"),
        "---\ndescription: d\nhooks:\n  PreToolUse:\n    - matcher: Bash\n      hooks:\n        - type: command\n          command: \"echo '{\\\"hookSpecificOutput\\\":{\\\"permissionDecision\\\":\\\"deny\\\",\\\"permissionDecisionReason\\\":\\\"guarded\\\"}}'\"\n---\nbody\n",
    )
    .unwrap();
    let skill = novacode_core::resources::skills(&[home.path().join("skills")])
        .0
        .remove(0);
    let hooks = Arc::new(novacode_core::plugin_hooks::PluginHooks::load(
        &[],
        work.path(),
        "s",
        None,
    ));
    let faux = Faux::new([
        tools_reply(vec![call("a", "bash", json!({"command": "touch made"}))]),
        FauxReply::text("done"),
    ]);
    let agent = Agent::new(
        Arc::new(faux.clone()),
        novacode_tools::builtins(false),
        Store::create(&Paths::new(home.path()), work.path(), true),
        cfg(work.path(), home.path()),
        Arc::new(LocalEnv),
    )
    .with_hooks(Arc::new(novacode_core::hooks::Hooks {
        handlers: vec![hooks.clone()],
    }))
    .with_plugin_hooks(hooks);
    let (agent, _) = drive_with(agent, &["go"], |h| {
        assert!(h.skill_activated(&skill).is_empty());
    })
    .await;
    let results: Vec<String> = agent
        .store
        .projection()
        .iter()
        .filter(|m| matches!(m, Message::ToolResult { .. }))
        .map(result_text)
        .collect();
    assert!(results[0].contains("guarded"), "{results:?}");
    assert!(!work.path().join("made").exists());
}

/// L7.14 check: a Claude Code plugin's hooks in a session. SessionStart text and
/// UserPromptSubmit context reach the model, a refused prompt never does, and a
/// Stop hook's block runs the agent once more with its reason.
#[tokio::test]
async fn claude_plugin_hooks_in_a_session() {
    let work = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let pkg = home.path().join(".novacode/packages/pony");
    std::fs::create_dir_all(pkg.join(".claude-plugin")).unwrap();
    let hook = |c: &str| json!([{"hooks": [{"type": "command", "command": c}]}]);
    std::fs::write(
        pkg.join(".claude-plugin/plugin.json"),
        json!({"name": "pony", "hooks": {
            "SessionStart": hook("cat >/dev/null; echo PONY MODE ON"),
            "UserPromptSubmit": hook(r#"grep -q forbidden && { echo 'not that' >&2; exit 2; }; echo '{"hookSpecificOutput":{"additionalContext":"prompt seen"}}'"#),
            "Stop": hook(r#"grep -q '"stop_hook_active":true' && exit 0; echo '{"decision":"block","reason":"check the tests first"}'"#),
        }})
        .to_string(),
    )
    .unwrap();
    let packages = vec![novacode_core::packages::Package {
        name: "pony".into(),
        dir: pkg,
        link: None,
        disabled: None,
    }];
    let hooks = novacode_core::plugin_hooks::PluginHooks::load(&packages, work.path(), "s", None);
    let faux = Faux::new([FauxReply::text("one"), FauxReply::text("two")]);
    let store = Store::create(&Paths::new(home.path()), work.path(), false);
    let agent = Agent::new(
        Arc::new(faux.clone()),
        vec![],
        store,
        cfg(work.path(), home.path()),
        Arc::new(LocalEnv),
    )
    .with_plugin_hooks(Arc::new(hooks));
    let (h, task) = agent::spawn(agent);
    let mut rx = h.subscribe();
    h.prompt("this is forbidden");
    let mut refused = false;
    loop {
        match rx.recv().await.unwrap() {
            AgentEvent::Stream(novacode_types::StreamEvent::Notice { text }) => refused |= text.contains("not that"),
            AgentEvent::Settled => break,
            _ => {}
        }
    }
    assert!(refused, "the refusal is shown");
    assert!(
        faux.requests.lock().unwrap().is_empty(),
        "a refused prompt makes no call"
    );
    h.prompt("hello");
    while !matches!(rx.recv().await.unwrap(), AgentEvent::Settled) {}
    h.shutdown();
    drop(task.await.unwrap());
    let reqs = faux.requests.lock().unwrap().clone();
    assert_eq!(reqs.len(), 2, "the Stop hook ran the agent once more");
    let first = format!("{:?}", reqs[0].messages);
    assert!(
        first.contains("PONY MODE ON") && first.contains("prompt seen"),
        "{first}"
    );
    assert!(!first.contains("forbidden"));
    assert!(format!("{:?}", reqs[1].messages).contains("check the tests first"));
}

/// `/reload` (L9): the rebuilt agent keeps the old one's live state.
#[tokio::test]
async fn reload_carries_reads_mode_and_session_approvals() {
    let work = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    std::fs::write(work.path().join("notes.txt"), "alpha\n").unwrap();
    let paths = Paths::new(home.path());
    let build = |faux: &Faux, store: Store| {
        let policy = Arc::new(novacode_core::policy::Policy::new(
            Default::default(),
            home.path().into(),
            None,
        ));
        let modes = novacode_core::modes::Modes::new(policy.clone(), Default::default());
        Agent::new(
            Arc::new(faux.clone()),
            novacode_tools::builtins(false),
            store,
            cfg(work.path(), home.path()),
            Arc::new(LocalEnv),
        )
        .with_shared_policy(policy)
        .with_modes(modes)
    };
    let faux = Faux::new([
        tools_reply(vec![call("c1", "read", json!({"path": "notes.txt"}))]),
        FauxReply::text("read it"),
    ]);
    let agent = build(&faux, Store::create(&paths, work.path(), false));
    let (agent, _) = drive(agent, &["read notes"]).await;
    agent.modes().unwrap().set(Some("plan")).unwrap();
    agent
        .policy()
        .allow_for_session(vec![novacode_core::policy::Rule::parse("Bash(make test:*)").unwrap()]);
    let carry = agent.carry();
    assert_eq!(carry.cwd, work.path());

    // Someone changes the file after the read: the reloaded agent still knows and says so.
    std::thread::sleep(std::time::Duration::from_millis(20));
    std::fs::write(work.path().join("notes.txt"), "alpha\n").unwrap();
    let faux2 = Faux::new([
        tools_reply(vec![call(
            "c2",
            "edit",
            json!({"path": "notes.txt", "edits": [{"old_text": "alpha", "new_text": "beta"}]}),
        )]),
        FauxReply::text("done"),
    ]);
    let fresh = build(&faux2, agent.store).adopt(carry);
    assert_eq!(fresh.modes().unwrap().active().as_deref(), Some("plan"));
    assert_eq!(fresh.policy().session_rules().len(), 1);
    fresh.modes().unwrap().set(None).unwrap();
    let (_fresh, _) = drive(fresh, &["now edit"]).await;
    let last = format!("{:?}", faux2.requests.lock().unwrap()[1].messages.last().unwrap());
    assert!(last.contains("changed on disk since your last read"), "{last}");
}

/// Fails `n` times with a transient error, then answers; returns the agent's events.
async fn transient_failures(n: usize) -> (Faux, Vec<AgentEvent>) {
    let work = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let mut script: Vec<FauxReply> = (0..n)
        .map(|_| FauxReply::Fail(ErrorKind::Transient, "overloaded".into()))
        .collect();
    script.push(FauxReply::text("ok"));
    let faux = Faux::new(script);
    let agent = Agent::new(
        Arc::new(faux.clone()),
        vec![],
        Store::create(&Paths::new(home.path()), work.path(), true),
        cfg(work.path(), home.path()),
        Arc::new(LocalEnv),
    )
    .with_retry_base(std::time::Duration::from_millis(5));
    let (_a, events) = drive(agent, &["hi"]).await;
    (faux, events)
}

#[tokio::test]
async fn loop_retries_four_times_then_succeeds() {
    let (faux, events) = transient_failures(4).await;
    let delays: Vec<u64> = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::Retry { delay_ms, .. } => Some(*delay_ms),
            _ => None,
        })
        .collect();
    assert_eq!(delays.len(), 4);
    assert_eq!(faux.requests.lock().unwrap().len(), 5);
    assert!(events.contains(&AgentEvent::AgentEnd { stop: StopReason::Stop }));
}

#[tokio::test]
async fn loop_gives_up_after_five_failures() {
    let (faux, events) = transient_failures(5).await;
    let retries = events.iter().filter(|e| matches!(e, AgentEvent::Retry { .. })).count();
    assert_eq!(retries, 4);
    assert_eq!(faux.requests.lock().unwrap().len(), 5);
    assert!(events.contains(&AgentEvent::AgentEnd {
        stop: StopReason::Error {
            kind: ErrorKind::Transient
        }
    }));
}

#[tokio::test]
async fn quoted_tool_markup_is_not_nudged() {
    let work = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let faux = Faux::new([
        FauxReply::text("The check looks for `<function_calls>` and <invoke name= in replies."),
        FauxReply::text("unused"),
    ]);
    let agent = Agent::new(
        Arc::new(faux.clone()),
        vec![],
        Store::create(&Paths::new(home.path()), work.path(), true),
        cfg(work.path(), home.path()),
        Arc::new(LocalEnv),
    );
    let (_a, events) = drive(agent, &["explain the check"]).await;
    assert_eq!(faux.requests.lock().unwrap().len(), 1, "no nudge, no second call");
    assert!(events.contains(&AgentEvent::AgentEnd { stop: StopReason::Stop }));
}

/// Never answers; signals when a card arrives.
struct Never(Arc<tokio::sync::Notify>);
impl novacode_core::policy::Approver for Never {
    fn approve(
        &self,
        _req: novacode_core::policy::ApprovalRequest,
    ) -> futures_util::future::BoxFuture<'_, novacode_core::policy::Approval> {
        self.0.notify_one();
        Box::pin(std::future::pending())
    }
}

/// An agent in ask mode whose first call waits on an approval that never comes.
fn waiting_on_approval(work: &Path, home: &Path) -> (Agent, Arc<tokio::sync::Notify>) {
    use novacode_core::policy::{Mode, Permissions, Policy};
    let faux = Faux::new([
        tools_reply(vec![call("a", "bash", json!({"command": "touch one"}))]),
        FauxReply::text("unused"),
    ]);
    let asked = Arc::new(tokio::sync::Notify::new());
    let agent = Agent::new(
        Arc::new(faux),
        novacode_tools::builtins(false),
        Store::create(&Paths::new(home), work, true),
        cfg(work, home),
        Arc::new(LocalEnv),
    )
    .with_policy(Policy::new(Permissions::default(), home.into(), None).with_mode(Mode::Ask))
    .with_approver(Arc::new(Never(asked.clone())));
    (agent, asked)
}

#[tokio::test]
async fn abort_ends_a_pending_approval() {
    let work = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let (agent, asked) = waiting_on_approval(work.path(), home.path());
    let (h, task) = agent::spawn(agent);
    let mut rx = h.subscribe();
    h.prompt("make a file");
    asked.notified().await;
    h.abort();
    let end = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if let AgentEvent::AgentEnd { stop } = rx.recv().await.unwrap() {
                return stop;
            }
        }
    })
    .await
    .expect("the run ends without an answer");
    assert_eq!(end, StopReason::Aborted);
    assert!(!work.path().join("one").exists(), "the tool did not run");
    h.shutdown();
    task.await.unwrap();
}

#[tokio::test]
async fn take_follow_ups_drops_queued_prompts() {
    let work = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let (agent, asked) = waiting_on_approval(work.path(), home.path());
    let (h, task) = agent::spawn(agent);
    let mut rx = h.subscribe();
    h.prompt("make a file");
    asked.notified().await;
    // The run is busy, so these wait in the follow-up queue.
    h.prompt("then this");
    h.prompt("and this");
    assert_eq!(h.take_follow_ups(), ["then this", "and this"]);
    assert!(h.take_follow_ups().is_empty());
    h.abort();
    while !matches!(rx.recv().await.unwrap(), AgentEvent::AgentEnd { .. }) {}
    h.shutdown();
    task.await.unwrap();
}
