//! TUI snapshot tests (M4 check): drive the app with agent events and keys,
//! render to a test backend, compare with `tests/snapshots/*.txt`.
//! `UPDATE_SNAPSHOTS=1 cargo test -p novacode-tui` rewrites them.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use novacode_tui::{App, Cmd, Init};
use novacode_types::*;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use serde_json::json;
use tokio::sync::mpsc;

fn app(history: Vec<Message>) -> (App, mpsc::UnboundedReceiver<Cmd>) {
    let (tx, rx) = mpsc::unbounded_channel();
    let init = Init {
        model: "claude/opus".into(),
        cwd: "/home/u/proj".into(),
        branch: Some("main".into()),
        window_tokens: 200_000,
        history,
    };
    (App::new(init, tx), rx)
}

fn screen(app: &mut App, w: u16, h: u16) -> String {
    let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
    t.draw(|f| app.draw(f)).unwrap();
    let buf = t.backend().buffer();
    (0..h)
        .map(|y| {
            let row: String = (0..w).map(|x| buf[(x, y)].symbol().to_string()).collect();
            row.trim_end().to_string()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn snapshot(name: &str, got: &str) {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(format!("tests/snapshots/{name}.txt"));
    if std::env::var_os("UPDATE_SNAPSHOTS").is_some() || !path.exists() {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, got).unwrap();
        return;
    }
    let want = std::fs::read_to_string(&path).unwrap();
    assert_eq!(got, want, "snapshot {name} differs; UPDATE_SNAPSHOTS=1 to accept");
}

fn assistant(content: Vec<Block>, stop: StopReason) -> Message {
    Message::Assistant(AssistantMessage {
        content,
        stop,
        usage: Usage {
            input: 10,
            output: 500,
            cache_read: 30_000,
            cache_write: 1000,
            reasoning: None,
        },
        provider: "claude".into(),
        model: "opus".into(),
        response_id: None,
        error_message: None,
    })
}

fn call(id: &str, name: &str, args: serde_json::Value) -> Block {
    Block::ToolCall(ToolCall {
        id: id.into(),
        name: name.into(),
        arguments: args,
        arguments_error: None,
        replay: None,
    })
}

fn result(id: &str, name: &str, text: &str, is_error: bool) -> Message {
    Message::ToolResult {
        call_id: id.into(),
        tool_name: name.into(),
        content: vec![Block::text(text)],
        is_error,
        details: None,
    }
}

/// One run: thinking, a bash call that fails, an edit, then the answer.
fn run_events(app: &mut App) {
    let ev = |a: &mut App, e| a.on_agent(e);
    ev(app, AgentEvent::AgentStart);
    ev(app, AgentEvent::Message(Message::user("fix the failing test")));
    ev(
        app,
        AgentEvent::Message(assistant(
            vec![
                Block::Thinking {
                    text: "look at the test output first".into(),
                    replay: None,
                },
                Block::text("Running the tests."),
                call(
                    "a",
                    "bash",
                    json!({"command": "cargo test", "intent": "see what fails"}),
                ),
            ],
            StopReason::ToolUse,
        )),
    );
    ev(
        app,
        AgentEvent::ToolStart {
            call: ToolCall {
                id: "a".into(),
                name: "bash".into(),
                arguments: json!({}),
                arguments_error: None,
                replay: None,
            },
        },
    );
    ev(
        app,
        AgentEvent::ToolUpdate {
            call_id: "a".into(),
            text: "\x1b[32mcompiling\x1b[0m\n".into(),
        },
    );
    ev(
        app,
        AgentEvent::Message(result(
            "a",
            "bash",
            "running 3 tests\ntest a ... ok\ntest b ... FAILED\nassertion failed: 1 == 2\n[exit code 101]",
            true,
        )),
    );
    ev(
        app,
        AgentEvent::ToolEnd {
            call_id: "a".into(),
            is_error: true,
            duration_ms: 2300,
        },
    );
    ev(
        app,
        AgentEvent::Message(assistant(
            vec![call("b", "edit", json!({"path": "src/lib.rs"}))],
            StopReason::ToolUse,
        )),
    );
    ev(
        app,
        AgentEvent::Message(result(
            "b",
            "edit",
            "Edited src/lib.rs\n12- assert_eq!(1, 2);\n12+ assert_eq!(2, 2);",
            false,
        )),
    );
    ev(
        app,
        AgentEvent::Message(assistant(
            vec![Block::text(
                "Fixed: the assertion in `src/lib.rs` compared **1** with 2.\n\n- tests pass now",
            )],
            StopReason::Stop,
        )),
    );
    ev(app, AgentEvent::AgentEnd { stop: StopReason::Stop });
    ev(app, AgentEvent::Settled);
}

#[test]
fn collapsed_and_expanded_turn() {
    let (mut a, _rx) = app(vec![]);
    run_events(&mut a);
    snapshot("collapsed", &screen(&mut a, 60, 16));
    a.on_key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::ALT));
    snapshot("expanded", &screen(&mut a, 60, 30));
}

#[test]
fn streaming_and_steering() {
    let (mut a, mut rx) = app(vec![]);
    a.on_agent(AgentEvent::AgentStart);
    a.on_agent(AgentEvent::Message(Message::user("explain")));
    a.on_agent(AgentEvent::Stream(StreamEvent::BlockStart {
        index: 0,
        kind: BlockKind::Thinking,
        name: None,
    }));
    a.on_agent(AgentEvent::Stream(StreamEvent::Delta {
        index: 0,
        text: "hmm let me".into(),
    }));
    a.on_agent(AgentEvent::Stream(StreamEvent::BlockStart {
        index: 1,
        kind: BlockKind::Text,
        name: None,
    }));
    a.on_agent(AgentEvent::Stream(StreamEvent::Delta {
        index: 1,
        text: "The loop has **three".into(),
    }));
    // Typing while busy steers; it shows as queued until delivered.
    for c in "use tests".chars() {
        a.on_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
    }
    a.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert_eq!(rx.try_recv().unwrap(), Cmd::Steer("use tests".into()));
    snapshot("streaming", &screen(&mut a, 50, 12));
    // Esc while busy aborts.
    a.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert_eq!(rx.try_recv().unwrap(), Cmd::Abort);
}

#[test]
fn resumed_history_and_narrow_resize() {
    let (mut a, mut rx) = app(vec![
        Message::user("hello"),
        assistant(
            vec![Block::text("Hi! How can I help with this project today?")],
            StopReason::Stop,
        ),
    ]);
    snapshot("resumed", &screen(&mut a, 30, 10));
    // History from the resumed session is on Up.
    a.on_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
    a.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert_eq!(rx.try_recv().unwrap(), Cmd::Prompt("hello".into()));
}

#[test]
fn interrupted_notice_survives_collapse() {
    let (mut a, _rx) = app(vec![]);
    a.on_agent(AgentEvent::AgentStart);
    a.on_agent(AgentEvent::Message(Message::user("run it")));
    a.on_agent(AgentEvent::Message(assistant(
        vec![call("a", "bash", json!({"command": "sleep 20"}))],
        StopReason::ToolUse,
    )));
    a.on_agent(AgentEvent::Message(result("a", "bash", "[cancelled]", true)));
    a.on_agent(AgentEvent::Message(assistant(vec![], StopReason::Aborted)));
    a.on_agent(AgentEvent::Settled);
    let s = screen(&mut a, 40, 8);
    assert!(
        s.contains("▸ 1 tool call · 1 failed") && s.contains("! interrupted"),
        "{s}"
    );
}
