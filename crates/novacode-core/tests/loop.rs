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
    let (h, task) = agent::spawn(agent);
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
    );
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
        notices[0].starts_with("cache miss: 20070 prompt tokens")
            && notices[0].contains("(evicted by the provider: prefix unchanged"),
        "{}",
        notices[0]
    );
    let log = std::fs::read_to_string(home.path().join("logs/cache-misses.jsonl")).unwrap();
    assert!(log.contains("\"lost\":20070"), "{log}");
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

/// When the observer can't run, the session model summarises the gap instead.
#[tokio::test]
async fn compaction_without_notes_falls_back() {
    use novacode_core::memory::{Settings, Worker};
    let work = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let big = "lorem ipsum ".repeat(300);
    let main = Faux::new((0..8).map(|i| FauxReply::text(&format!("answer {i} {big}"))));
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
    let p = format!("{:?}", agent.store.projection());
    assert!(p.contains("Summary of the earlier conversation"), "{p}");
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
    assert_eq!(results[0], "both read\npush refused");
    assert!(
        results[1].contains("Script failed: Error: stop") && results[1].contains("2 file(s) restored"),
        "{}",
        results[1]
    );
    assert_eq!(std::fs::read_to_string(work.path().join("a.txt")).unwrap(), "alpha");
    assert!(!work.path().join("new.txt").exists());
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
