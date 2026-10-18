//! TUI snapshot tests (M4 check): drive the app with agent events and keys,
//! render to a test backend, compare with `tests/snapshots/*.txt`.
//! `UPDATE_SNAPSHOTS=1 cargo test -p novacode-tui` rewrites them.

use crossterm::event::{Event, MouseButton, MouseEvent, MouseEventKind};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use novacode_tui::dialog::{Answer, ApproveReq, PickItem};
use novacode_tui::{App, Cmd, Exit, Init, UiEvent};
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
        keymap: Default::default(),
        collapse_turns: true,
        info: vec![],
        notices: vec![],
        theme: Default::default(),
        thinking: Default::default(),
        level: Default::default(),
        price: None,
        spent: 0.0,
        commands: vec![],
        draft: String::new(),
        attached: false,
        statusline: Default::default(),
        panel_only: false,
        panel_auto: false,
        nerd_icons: false,
        slow: false,
        images: "off".into(),
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
        thinking_level: None,
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
fn back_to_back_thinking_reads_as_one_block() {
    let (mut a, _rx) = app(vec![]);
    a.on_agent(AgentEvent::Message(Message::user("go")));
    let th = |t: &str| Block::Thinking {
        text: t.into(),
        replay: None,
    };
    a.on_agent(AgentEvent::Message(assistant(
        vec![
            th("Checking the doc revealed.\n\nI spot an error."),
            th("The rows aren't related."),
            Block::text("done"),
        ],
        StopReason::Stop,
    )));
    a.on_agent(AgentEvent::Settled);
    a.on_key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::ALT));
    let folded = screen(&mut a, 60, 20);
    assert_eq!(folded.matches('∴').count(), 1, "{folded}");
    a.on_key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL));
    let full = screen(&mut a, 60, 20);
    assert_eq!(full.matches('∴').count(), 1, "{full}");
    assert!(
        full.contains("revealed.") && !full.contains("revealed.I") && full.contains("  I spot"),
        "{full}"
    );
}

#[test]
fn tools_show_like_pi() {
    let (mut a, _rx) = app(vec![]);
    a.on_agent(AgentEvent::AgentStart);
    a.on_agent(AgentEvent::Message(Message::user("ask and spawn")));
    let report = (1..=14)
        .map(|i| format!("report line {i}"))
        .collect::<Vec<_>>()
        .join("\n");
    for (id, name, args, out) in [
        (
            "q",
            "ask_user",
            json!({"questions": [{"question": "Which database?", "options": [{"label": "pg"}]}, {"question": "Name?", "options": []}]}),
            "Q: Which database?\nA: pg\nQ: Name?\nA: nova".to_string(),
        ),
        ("s", "agent", json!({"action": "read", "id": "a10"}), report),
        (
            "c",
            "codemode",
            json!({"code": "const a = await tools.read({path: 'x'});\ntext(a);"}),
            "hello from x".to_string(),
        ),
    ] {
        a.on_agent(AgentEvent::Message(assistant(
            vec![call(id, name, args)],
            StopReason::ToolUse,
        )));
        a.on_agent(AgentEvent::Message(result(id, name, &out, false)));
    }
    a.on_agent(AgentEvent::Settled);
    a.on_key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::ALT));
    snapshot("tools_like_pi", &screen(&mut a, 70, 40));
    a.on_key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL));
    let full = screen(&mut a, 70, 60);
    assert!(
        full.contains("report line 14") && !full.contains("more lines"),
        "{full}"
    );
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
    // The run loop steps the pacer once per frame.
    while a.pace() {}
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
        s.contains("▸ 1 tool call · 1 failed") && s.contains("· interrupted"),
        "{s}"
    );
}

fn key(a: &mut App, code: KeyCode, m: KeyModifiers) {
    a.on_key(KeyEvent::new(code, m));
}

#[test]
fn approval_card_blocks_input_and_answers() {
    let (mut a, _rx) = app(vec![]);
    a.on_agent(AgentEvent::AgentStart);
    a.on_agent(AgentEvent::Message(Message::user("clean")));
    let (tx, mut reply) = tokio::sync::oneshot::channel();
    a.on_ui(UiEvent::Approve(ApproveReq {
        id: 1,
        tool: "bash".into(),
        detail: "rm -rf target".into(),
        reason: "rm -r deletes files".into(),
        rule: "rm -rf target".into(),
        risky: true,
        reply: tx,
    }));
    snapshot("approval", &screen(&mut a, 60, 14));
    // Typing goes to the card, not the editor.
    key(&mut a, KeyCode::Char('x'), KeyModifiers::NONE);
    assert!(a.editor.is_empty());
    key(&mut a, KeyCode::Char('a'), KeyModifiers::NONE);
    assert_eq!(reply.try_recv().unwrap(), Answer::Session);
    let s = screen(&mut a, 60, 14);
    assert!(s.contains("allowed for this session: bash rm -rf target"), "{s}");
}

#[test]
fn slash_commands_pickers_and_pager() {
    let (mut a, mut rx) = app(vec![]);
    run_events(&mut a);
    for c in "/model claude/haiku".chars() {
        key(&mut a, KeyCode::Char(c), KeyModifiers::NONE);
    }
    key(&mut a, KeyCode::Enter, KeyModifiers::NONE);
    assert_eq!(rx.try_recv().unwrap(), Cmd::SetModel("claude/haiku".into()));
    a.on_ui(UiEvent::ModelSet {
        model: "claude/haiku".into(),
        window_tokens: 200_000,
        level: ThinkingLevel::Medium,
        price: None,
    });
    let s = screen(&mut a, 60, 30);
    assert!(s.contains("model → claude/haiku · medium"), "{s}");
    // Ctrl+L asks for models; the picker filters and picks.
    key(&mut a, KeyCode::Char('l'), KeyModifiers::CONTROL);
    assert_eq!(rx.try_recv().unwrap(), Cmd::ListModels);
    let items = ["opus", "sonnet"]
        .map(|m| PickItem {
            label: m.into(),
            detail: String::new(),
            value: format!("claude/{m}"),
        })
        .to_vec();
    a.on_ui(UiEvent::Models { result: Ok(items) });
    snapshot("model_picker", &screen(&mut a, 60, 16));
    key(&mut a, KeyCode::Char('s'), KeyModifiers::NONE);
    key(&mut a, KeyCode::Enter, KeyModifiers::NONE);
    assert_eq!(rx.try_recv().unwrap(), Cmd::SetModel("claude/sonnet".into()));
    // Ctrl+O shows every tool's output in full, inline; again folds it.
    key(&mut a, KeyCode::Char('o'), KeyModifiers::CONTROL);
    assert!(a.tr.tools_full);
    key(&mut a, KeyCode::Char('o'), KeyModifiers::CONTROL);
    // Alt+/ lists keys, filterable.
    key(&mut a, KeyCode::Char('/'), KeyModifiers::ALT);
    for c in "model".chars() {
        key(&mut a, KeyCode::Char(c), KeyModifiers::NONE);
    }
    let s = screen(&mut a, 80, 16);
    assert!(s.contains("app.model.select") && !s.contains("app.exit"), "{s}");
    key(&mut a, KeyCode::Esc, KeyModifiers::NONE);
    // /resume asks for sessions; picking one exits to switch.
    for c in "/resume".chars() {
        key(&mut a, KeyCode::Char(c), KeyModifiers::NONE);
    }
    key(&mut a, KeyCode::Enter, KeyModifiers::NONE);
    assert_eq!(rx.try_recv().unwrap(), Cmd::ListSessions);
    a.on_ui(UiEvent::Sessions {
        items: vec![PickItem {
            label: "old work".into(),
            detail: "3 prompts".into(),
            value: "/s/1.jsonl".into(),
        }],
    });
    key(&mut a, KeyCode::Enter, KeyModifiers::NONE);
    assert_eq!(a.exit, Some(Exit::Resume("/s/1.jsonl".into())));
}

#[test]
fn history_search() {
    let (mut a, mut rx) = app(vec![Message::user("cargo test -p core"), Message::user("git status")]);
    key(&mut a, KeyCode::Char('r'), KeyModifiers::CONTROL);
    for c in "carg".chars() {
        key(&mut a, KeyCode::Char(c), KeyModifiers::NONE);
    }
    assert_eq!(a.editor.text(), "cargo test -p core");
    key(&mut a, KeyCode::Enter, KeyModifiers::NONE);
    assert!(rx.try_recv().is_err(), "Enter accepts the match, it doesn't send");
    key(&mut a, KeyCode::Enter, KeyModifiers::NONE);
    assert_eq!(rx.try_recv().unwrap(), Cmd::Prompt("cargo test -p core".into()));
}

fn typ(a: &mut App, s: &str) {
    for c in s.chars() {
        key(a, KeyCode::Char(c), KeyModifiers::NONE);
    }
}

#[test]
fn slash_and_file_completion() {
    let (mut a, mut rx) = app(vec![]);
    typ(&mut a, "/mo");
    assert!(screen(&mut a, 60, 12).contains("▸ /model"));
    key(&mut a, KeyCode::Tab, KeyModifiers::NONE);
    assert_eq!(a.editor.text(), "/model ");
    a.editor.clear();
    // @file completion lists this crate's files (cwd = /home/u/proj doesn't exist → no popup).
    typ(&mut a, "look at @zzz");
    assert!(!screen(&mut a, 60, 12).contains("▸"));
    // Enter on a complete command sends it.
    a.editor.clear();
    typ(&mut a, "/new");
    key(&mut a, KeyCode::Enter, KeyModifiers::NONE);
    assert_eq!(a.exit, Some(Exit::New));
    assert!(rx.try_recv().is_err());
}

#[test]
fn file_completion_in_a_real_folder() {
    let d = tempfile_dir();
    std::fs::create_dir_all(d.join("src")).unwrap();
    std::fs::write(d.join("src/main.rs"), "").unwrap();
    std::fs::write(d.join("README.md"), "").unwrap();
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut a = App::new(
        Init {
            model: "claude/opus".into(),
            cwd: d.display().to_string(),
            branch: None,
            window_tokens: 200_000,
            history: vec![],
            keymap: Default::default(),
            collapse_turns: true,
            info: vec![],
            notices: vec![],
            theme: Default::default(),
            thinking: Default::default(),
            level: Default::default(),
            price: None,
            spent: 0.0,
            commands: vec![],
            draft: String::new(),
            attached: false,
            statusline: Default::default(),
            panel_only: false,
            panel_auto: false,
            nerd_icons: false,
            slow: false,
            images: "off".into(),
        },
        tx,
    );
    // Outside the project: `@../` lists the parent folder, filtered by the typed prefix.
    let sibling = d.file_name().unwrap().to_string_lossy().into_owned();
    typ(&mut a, &format!("@../{sibling}"));
    assert!(screen(&mut a, 80, 12).contains(&format!("{sibling}/")));
    a.editor.clear();
    typ(&mut a, "@./sr");
    key(&mut a, KeyCode::Tab, KeyModifiers::NONE);
    assert_eq!(a.editor.text(), "@./src/");
    assert!(
        screen(&mut a, 60, 12).contains("▸ main.rs"),
        "a folder opens straight away"
    );
    a.editor.clear();
    typ(&mut a, "see @mai");
    assert!(screen(&mut a, 60, 12).contains("▸ src/main.rs"));
    key(&mut a, KeyCode::Enter, KeyModifiers::NONE);
    assert_eq!(a.editor.text(), "see @src/main.rs ");
}

fn tempfile_dir() -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("nc-tui-{}-{}", std::process::id(), line!()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

#[test]
fn transcript_search_and_thinking_toggle() {
    let (mut a, _rx) = app(vec![]);
    run_events(&mut a);
    screen(&mut a, 60, 30);
    key(&mut a, KeyCode::Char('f'), KeyModifiers::CONTROL);
    typ(&mut a, "tests pass");
    let s = screen(&mut a, 60, 30);
    assert!(s.contains("search: tests pass  1/1"), "{s}");
    key(&mut a, KeyCode::Esc, KeyModifiers::NONE);
    // Ctrl+T shows thinking in full (the turn must be expanded to see it).
    key(&mut a, KeyCode::Char('o'), KeyModifiers::ALT);
    key(&mut a, KeyCode::Char('t'), KeyModifiers::CONTROL);
    assert!(screen(&mut a, 60, 30).contains("∴ look at the test output first"));
}

#[test]
fn stream_pacer_reveals_gradually() {
    let (mut a, _rx) = app(vec![]);
    a.on_agent(AgentEvent::AgentStart);
    a.on_agent(AgentEvent::Message(Message::user("go")));
    a.on_agent(AgentEvent::Stream(StreamEvent::BlockStart {
        index: 0,
        kind: BlockKind::Text,
        name: None,
    }));
    a.on_agent(AgentEvent::Stream(StreamEvent::Delta {
        index: 0,
        text: "word ".repeat(200),
    }));
    assert!(a.pace(), "1000 chars can't all show in one frame");
    let mut frames = 1;
    while a.pace() {
        frames += 1;
    }
    assert!((5..60).contains(&frames), "caught up in {frames} frames");
}

#[test]
fn thinking_level_cycle_and_picker() {
    let (mut a, mut rx) = app(vec![]);
    key(&mut a, KeyCode::BackTab, KeyModifiers::SHIFT);
    assert_eq!(rx.try_recv().unwrap(), Cmd::SetThinking(ThinkingLevel::High));
    // The host answers with the level it really uses.
    a.on_ui(UiEvent::Thinking {
        level: ThinkingLevel::High,
        note: "thinking → high from the next message; the cache is kept".into(),
    });
    let s = screen(&mut a, 70, 12);
    assert!(
        s.contains("claude/opus · high") && s.contains("the cache is kept"),
        "{s}"
    );
    typ(&mut a, "/thinking");
    key(&mut a, KeyCode::Enter, KeyModifiers::NONE);
    assert!(screen(&mut a, 70, 16).contains("Thinking level"));
    typ(&mut a, "xh");
    key(&mut a, KeyCode::Enter, KeyModifiers::NONE);
    assert_eq!(rx.try_recv().unwrap(), Cmd::SetThinking(ThinkingLevel::Xhigh));
}

#[test]
fn user_message_style_setting() {
    let (mut a, _rx) = app(vec![Message::user("hi")]);
    assert!(screen(&mut a, 30, 8).contains("╭────"), "rounded by default");
    let warn = a
        .theme
        .apply_user_message(&json!({ "border": "double", "paddingX": 2 }));
    assert!(warn.is_none());
    a.tr.turns[0].touch();
    let s = screen(&mut a, 30, 8);
    assert!(s.contains("╔═") && s.contains("║  hi"), "{s}");
    let warn = a
        .theme
        .apply_user_message(&json!({ "border": "wavy", "text": { "fg": "#ffffff", "bg": "#333333" } }));
    assert_eq!(warn.as_deref(), Some("settings ui.userMessage: ignored bad border"));
    a.theme.apply_user_message(&json!({ "border": "none", "paddingX": 0 }));
    a.tr.turns[0].touch();
    let s = screen(&mut a, 30, 8);
    assert!(s.starts_with(" hi"), "no frame: {s}");
}

#[test]
fn folded_thinking_names_its_key() {
    let (mut a, _rx) = app(vec![]);
    run_events(&mut a);
    key(&mut a, KeyCode::Char('o'), KeyModifiers::ALT);
    let s = screen(&mut a, 70, 30);
    assert!(s.contains("∴ thought · 6 words · ctrl+t to show"), "{s}");
}

#[test]
fn provider_notice_is_shown() {
    let (mut a, _rx) = app(vec![]);
    a.on_agent(AgentEvent::AgentStart);
    a.on_agent(AgentEvent::Message(Message::user("go")));
    a.on_agent(AgentEvent::Stream(StreamEvent::Notice {
        text: "claude account `default` is exhausted (quota); switched to account `work`".into(),
    }));
    let s = screen(&mut a, 90, 10);
    assert!(s.contains("switched to account `work`"), "{s}");
}

#[test]
fn cost_shows_only_with_api_prices() {
    let (mut a, _rx) = app(vec![]);
    let reply = || {
        let mut m = assistant(vec![Block::text("ok")], StopReason::Stop);
        if let Message::Assistant(x) = &mut m {
            x.usage = novacode_types::Usage {
                input: 100_000,
                output: 10_000,
                ..Default::default()
            };
        }
        m
    };
    a.on_agent(AgentEvent::Message(reply()));
    assert!(!screen(&mut a, 100, 8).contains('$'), "subscription: no cost");
    a.on_ui(UiEvent::ModelSet {
        model: "anthropic/sonnet".into(),
        window_tokens: 200_000,
        level: ThinkingLevel::Medium,
        price: Some(novacode_types::Cost {
            input: 3.0,
            output: 15.0,
            ..Default::default()
        }),
    });
    a.on_agent(AgentEvent::Message(reply()));
    let s = screen(&mut a, 100, 8);
    assert!(s.contains("$0.45"), "{s}");
}

#[test]
fn side_panel_extension_ui_and_keys() {
    let (mut a, mut rx) = app(vec![]);
    let ui = |a: &mut App, ext: &str, op: serde_json::Value| {
        a.on_agent(AgentEvent::Ui {
            ext: ext.into(),
            op: serde_json::from_value(op).unwrap(),
        })
    };
    a.on_agent(AgentEvent::AgentStart);
    a.on_agent(AgentEvent::Message(Message::ToolResult {
        call_id: "t".into(),
        tool_name: "todo".into(),
        content: vec![],
        is_error: false,
        details: Some(json!({"todo": [
            {"id": 1, "text": "read the code", "status": "done"},
            {"id": 2, "text": "write the panel", "status": "active", "note": "rendering"},
            {"id": 3, "text": "test it", "status": "pending"}
        ]})),
    }));
    ui(
        &mut a,
        "deploy",
        json!({"op": "status", "key": "deploy", "text": "▲ deploying", "style": "warn"}),
    );
    ui(
        &mut a,
        "deploy",
        json!({"op": "widget", "key": "w", "lines": [[{"text": "build 3/5", "style": "accent"}]]}),
    );
    ui(&mut a, "deploy", json!({"op": "editor", "title": "deploy mode"}));
    ui(&mut a, "deploy", json!({"op": "shortcut", "key": "ctrl+alt+d"}));
    key(&mut a, KeyCode::Char('p'), KeyModifiers::CONTROL | KeyModifiers::ALT);
    snapshot("panel_agent_tab", &screen(&mut a, 110, 16));
    // Focus the panel; a region tab with higher priority takes it and gets the keys.
    key(&mut a, KeyCode::Char('f'), KeyModifiers::CONTROL | KeyModifiers::ALT);
    ui(
        &mut a,
        "deploy",
        json!({"op": "open_tab", "name": "deploy", "title": "Deploy", "priority": 30, "region": true}),
    );
    ui(
        &mut a,
        "deploy",
        json!({"op": "region", "tab": "deploy", "lines": ["region line", [{"text": "ok", "style": "ok"}]]}),
    );
    key(&mut a, KeyCode::Left, KeyModifiers::NONE);
    match rx.try_recv() {
        Ok(Cmd::ExtKey { ext, tab, key, .. }) => assert_eq!(
            (ext.as_str(), tab.as_deref(), key.as_str()),
            ("deploy", Some("deploy"), "left")
        ),
        other => panic!("{other:?}"),
    }
    snapshot("panel_region_tab", &screen(&mut a, 110, 16));
    key(&mut a, KeyCode::Esc, KeyModifiers::NONE);
    // Back in the editor: an extension shortcut goes to its extension.
    key(&mut a, KeyCode::Char('d'), KeyModifiers::CONTROL | KeyModifiers::ALT);
    assert!(matches!(rx.try_recv(), Ok(Cmd::ExtKey { ext, tab: None, .. }) if ext == "deploy"));
    // Narrow screens keep the panel hidden.
    let narrow = screen(&mut a, 80, 16);
    assert!(!narrow.contains("│  Agent"), "no panel: {narrow}");
    assert!(
        narrow.contains("Todos 1/3") && narrow.contains("next ○ test it"),
        "the compact todo widget instead: {narrow}"
    );
    // A header, and a tool that draws its own result lines.
    ui(
        &mut a,
        "deploy",
        json!({"op": "header", "lines": [[{"text": "DEPLOY MODE", "style": "error"}]]}),
    );
    a.on_agent(AgentEvent::Message(assistant(
        vec![call("d1", "deploy_status", json!({}))],
        StopReason::ToolUse,
    )));
    a.on_agent(AgentEvent::Message(Message::ToolResult {
        call_id: "d1".into(),
        tool_name: "deploy_status".into(),
        content: vec![Block::text("raw text the model reads")],
        is_error: false,
        details: Some(json!({"ui": [[{"text": "✓ ", "style": "ok"}, {"text": "prod is healthy"}]]})),
    }));
    let s = screen(&mut a, 80, 16);
    assert!(s.lines().next().unwrap().contains("DEPLOY MODE"), "{s}");
    assert!(
        s.contains("✓ prod is healthy") && !s.contains("raw text the model reads"),
        "{s}"
    );
}

#[cfg(feature = "images")]
#[test]
fn images_draw_in_reserved_rows() {
    const PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAFAAAAAoCAIAAADmAupWAAAAZ0lEQVR4nO3ayQ0AMAwCQfpvGpfBwyPlb0U5jBfSZLJGZbsqa8NO2JX2hn1afmltSR8mPCgt0pKWNjyYloyHAADiAfFgWiAeagnT4tJAPOeB1cJbYqZxD/nDDHEJAJEHGQ+hlnxN8RyI7HQ1MYqMpQAAAABJRU5ErkJggg==";
    let (mut a, _rx) = app(vec![Message::User {
        content: vec![
            Block::text("look"),
            Block::Image {
                mime_type: "image/png".into(),
                data: PNG.into(),
            },
        ],
    }]);
    let before = screen(&mut a, 60, 14);
    assert!(
        before.contains("[image 80×40]"),
        "a placeholder without a protocol: {before}"
    );
    a.set_images(novacode_tui::images::Images::halfblocks());
    let draw = |a: &mut App| {
        let mut t = Terminal::new(TestBackend::new(60, 14)).unwrap();
        t.draw(|f| a.draw(f)).unwrap();
        let buf = t.backend().buffer().clone();
        let text: Vec<String> = (0..14)
            .map(|y| (0..60).map(|x| buf[(x, y)].symbol().to_string()).collect())
            .collect();
        let painted = (0..14)
            .flat_map(|y| (0..60).map(move |x| (x, y)))
            .filter(|&(x, y)| buf[(x, y)].bg != ratatui::style::Color::Reset)
            .count();
        (text, painted)
    };
    let (text, painted) = draw(&mut a);
    let row = text.iter().position(|l| l.contains("▸ image 80×40 · click to show"));
    assert!(row.is_some() && painted < 10, "folded by default: {text:#?}");
    let click = |a: &mut App, row: usize| {
        for kind in [
            MouseEventKind::Down(MouseButton::Left),
            MouseEventKind::Up(MouseButton::Left),
        ] {
            a.on_event(Event::Mouse(MouseEvent {
                kind,
                column: 3,
                row: row as u16,
                modifiers: KeyModifiers::NONE,
            }));
        }
    };
    click(&mut a, row.unwrap());
    let (text, painted) = draw(&mut a);
    assert!(
        painted >= 10,
        "a click shows it, drawn with half-blocks ({painted}): {text:#?}"
    );
    let row = text
        .iter()
        .position(|l| l.contains("▾ image 80×40"))
        .expect("the fold line stays");
    click(&mut a, row);
    assert!(draw(&mut a).1 < 10, "a second click folds it");
}
