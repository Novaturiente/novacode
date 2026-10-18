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
        thinking: Default::default(),
        spill_dir: home.join("spill"),
        redactor: Default::default(),
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
    // Held, not run: the result is the refusal, not bash output.
    assert!(
        r[1].contains("Held by the safety check") && r[1].contains("justification"),
        "{}",
        r[1]
    );
    assert!(r[2].contains("git push"), "{}", r[2]);
    assert!(
        r[3].contains("[redacted: claude login]") && !r[3].contains("SECRET-TOKEN"),
        "{}",
        r[3]
    );
    // With a justification the held command runs (headless).
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
