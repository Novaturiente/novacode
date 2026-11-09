//! TUI snapshot tests (M4 check): drive the app with agent events and keys,
//! render to a test backend, compare with `tests/snapshots/*.txt`.
//! `UPDATE_SNAPSHOTS=1 cargo test -p novacode-tui` rewrites them.

use crossterm::event::{Event, MouseButton, MouseEvent, MouseEventKind};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use novacode_tui::app::Focus;
use novacode_tui::dialog::{Answer, ApproveReq, PickItem};
use novacode_tui::{App, Cmd, Exit, Init, UiEvent};
use novacode_types::*;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use serde_json::json;
use tokio::sync::mpsc;

fn app(history: Vec<Message>) -> (App, mpsc::UnboundedReceiver<Cmd>) {
    app_at(history, None)
}

/// `attached`: the session id of an attach window.
fn app_at(history: Vec<Message>, attached: Option<&str>) -> (App, mpsc::UnboundedReceiver<Cmd>) {
    let (tx, rx) = mpsc::unbounded_channel();
    (App::new(init(history, attached), tx), rx)
}

/// Every test app starts with a frozen clock, so times and ages on screen never drift.
fn init(history: Vec<Message>, attached: Option<&str>) -> Init {
    novacode_tui::clock::freeze();
    Init {
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
        compactions: 0,
        commands: vec![],
        draft: String::new(),
        attached: attached.map(String::from),
        statusline: Default::default(),
        panel_only: false,
        panel_auto: false,
        nerd_icons: false,
        slow: false,
        images: "off".into(),
        procs: Vec::new(),
    }
}

/// An empty app with Nerd (`true`) or plain icons, for golden screens.
fn app_icons(nerd: bool) -> (App, mpsc::UnboundedReceiver<Cmd>) {
    let (tx, rx) = mpsc::unbounded_channel();
    let mut i = init(vec![], None);
    i.nerd_icons = nerd;
    (App::new(i, tx), rx)
}

/// Golden screen (TP2): compares the screen text with `tests/golden/<name>.txt`.
/// A missing file fails; `UPDATE_GOLDEN=1 cargo test -p novacode-tui` (re)writes them.
fn golden(name: &str, got: &str) {
    let got = unversioned(got);
    let got = got.as_str();
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(format!("tests/golden/{name}.txt"));
    if std::env::var_os("UPDATE_GOLDEN").is_some() {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, got).unwrap();
        return;
    }
    let want = std::fs::read_to_string(&path)
        .unwrap_or_else(|_| panic!("golden {name} missing; UPDATE_GOLDEN=1 to write it\n{got}"));
    assert_eq!(got, want, "golden {name} differs; UPDATE_GOLDEN=1 to accept");
}

/// Every golden screen is drawn at 80 and 120 columns, with plain and Nerd icons.
fn goldens(name: &str, h: u16, setup: impl Fn(&mut App)) {
    for nerd in [false, true] {
        for w in [80, 120] {
            let (mut a, _rx) = app_icons(nerd);
            setup(&mut a);
            let icons = if nerd { "nerd" } else { "plain" };
            golden(&format!("{name}_{w}_{icons}"), &screen(&mut a, w, h));
        }
    }
}

/// TP2 baseline: the Tasks tab with a long title, a project tag, a 3-hour age, several
/// running rows and the second row selected. Below 100 columns the panel is hidden.
#[test]
fn golden_tasks_tab() {
    let ago = |s: u64| novacode_tui::clock::now_ms() - s * 1000 - 20_000;
    let row = |label: &str, repo: &str, secs: u64| novacode_tui::panel::TaskRow {
        label: label.into(),
        repo: repo.into(),
        state: "running".into(),
        model: "claude/claude-sonnet-5-5".into(),
        tokens: 41_200,
        started_ms: ago(secs),
        session: Some(format!("01{label}")),
        ..Default::default()
    };
    goldens("tasks_tab", 24, |a| {
        a.on_ui(UiEvent::Board {
            tasks: vec![
                row("T3-fix-the-parser-for-nested-generic-arguments", "novacode", 3 * 3600),
                row("T4-docs-site-search", "site", 2 * 3600),
                row("T5-api-retry-backoff", "api", 3600),
            ],
            needs: vec![],
        });
        a.panel.visible = true;
        a.panel.show_tab("tasks");
        a.set_focus(Focus::Panel);
        key(a, KeyCode::Down, KeyModifiers::NONE);
    });
}

/// TP10 after-capture bugs at 120 columns: a title takes the room a short tag leaves
/// (tags stay aligned on their right edge), and a task's detail shows the model's short
/// name, so it fits on one line. Detail lines wrap only at ` · `; a part longer than
/// the line is cut with `…`.
#[test]
fn panel_rows_fill_title_and_detail_room() {
    let ago = |s: u64| novacode_tui::clock::now_ms() - s * 1000 - 20_000;
    let row = |label: &str, repo: &str, model: &str| novacode_tui::panel::TaskRow {
        label: label.into(),
        repo: repo.into(),
        state: "running".into(),
        model: model.into(),
        tokens: 41_200,
        started_ms: ago(3600),
        ..Default::default()
    };
    let sonnet = "claude/claude-sonnet-5-5";
    let (mut a, _rx) = app(vec![]);
    a.on_ui(UiEvent::Board {
        tasks: vec![
            row("T3-fix-the-parser-for-nested-generic-arguments", "novacode", sonnet),
            row("T4-docs-site-search", "site", sonnet),
            row("T5-api-retry-backoff", "api", sonnet),
            row("T6-wide", "api", "openai/gpt-6-codex-max-preview"),
            row("T7-long", "api", "x/averyveryverylongmodelnamewithoutbreaks-preview"),
        ],
        needs: vec![],
    });
    a.panel.visible = true;
    a.panel.show_tab("tasks");
    let s = screen(&mut a, 120, 30);
    let lines: Vec<&str> = s.lines().collect();
    let at = |id: &str| {
        lines
            .iter()
            .position(|l| l.contains(id))
            .unwrap_or_else(|| panic!("{id}\n{s}"))
    };
    let (t4, t5) = (lines[at("T4-")], lines[at("T5-")]);
    assert!(
        t4.contains("T4-docs-site-search "),
        "the title uses the free room: {t4:?}"
    );
    assert!(
        t5.contains("T5-api-retry-backoff "),
        "the title uses the free room: {t5:?}"
    );
    let end = |l: &str, tag: &str| l[..l.rfind(tag).unwrap()].chars().count() + tag.chars().count();
    assert_eq!(
        end(t4, " site "),
        end(lines[at("T3-")], " novacode "),
        "tags share a right edge"
    );
    assert_eq!(end(t5, " api "), end(t4, " site "), "tags share a right edge");
    // Two rows per task: the main line and one detail line with the short model name.
    for (id, next) in [("T4-", "T5-"), ("T5-", "T6-")] {
        let d = lines[at(id) + 1];
        assert!(d.ends_with("running · sonnet-5-5 · 41.2k"), "{d:?}\n{s}");
        assert!(lines[at(id) + 2].contains(next), "one detail line\n{s}");
    }
    // Too wide for one line: it wraps at ` · ` only, never inside `gpt-6-codex-…`.
    let t6 = at("T6-");
    assert!(lines[t6 + 1].ends_with("│    running"), "{:?}\n{s}", lines[t6 + 1]);
    assert!(
        lines[t6 + 2].ends_with("│    gpt-6-codex-max-preview · 41.2k"),
        "{:?}\n{s}",
        lines[t6 + 2]
    );
    // One part longer than the line is cut with `…`.
    let t7 = at("T7-");
    assert!(lines[t7 + 1].ends_with("    running"), "{s}");
    assert!(lines[t7 + 2].contains("    averyvery"), "{s}");
    assert!(lines[t7 + 2].ends_with('…'), "{s}");
}

/// A drawn frame's cells, for colour checks. Any motion (TS4) is finished first, as a
/// real screen would be a few frames later; [`draw_live`] draws mid-motion.
fn draw(a: &mut App, w: u16, h: u16) -> ratatui::buffer::Buffer {
    a.settle();
    draw_live(a, w, h)
}

/// One frame at the frozen clock's time, motion left running.
fn draw_live(a: &mut App, w: u16, h: u16) -> ratatui::buffer::Buffer {
    let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
    t.draw(|f| a.draw(f)).unwrap();
    t.backend().buffer().clone()
}

/// A drawn frame's text, one line per row, trailing spaces cut.
fn text(buf: &ratatui::buffer::Buffer) -> String {
    (0..buf.area.height)
        .map(|y| {
            let row: String = (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect();
            row.trim_end().to_string()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Three running tasks: a long title, a long project name, and short ones.
fn board(a: &mut App) {
    let ago = |s: u64| novacode_tui::clock::now_ms() - s * 1000 - 20_000;
    let row = |label: &str, repo: &str, secs: u64| novacode_tui::panel::TaskRow {
        label: label.into(),
        repo: repo.into(),
        state: "running".into(),
        model: "claude/claude-sonnet-5-5".into(),
        tokens: 41_200,
        started_ms: ago(secs),
        session: Some(format!("01{label}")),
        ..Default::default()
    };
    a.on_ui(UiEvent::Board {
        tasks: vec![
            row("T3-fix-the-parser-for-nested-generic-arguments", "novacode", 3 * 3600),
            row("T4-docs", "a-very-long-project-name", 2 * 3600),
            row("T5-api", "api", 3600),
        ],
        needs: vec![],
    });
    a.panel.visible = true;
    a.panel.show_tab("tasks");
}

/// TP4: title, tag and age sit in fixed columns with a 2-cell gap before the age, on a
/// narrow (40) and a wide (60) panel. The title shrinks first; a long tag is capped.
#[test]
fn panel_rows_keep_fixed_columns() {
    for w in [120u16, 240] {
        let (mut a, _rx) = app(vec![]);
        board(&mut a);
        let s = screen(&mut a, w, 24);
        let edge = (w - a.panel.width(w)) as usize;
        let rows: Vec<String> = ["T3-", "T4-", "T5-"]
            .iter()
            .map(|id| {
                let l = s.lines().find(|l| l.contains(id)).unwrap_or_else(|| panic!("{s}"));
                l.chars().skip(edge).collect()
            })
            .collect();
        let panel_w = a.panel.width(w) as usize;
        // Tags are right-aligned: each ends where the 2-cell gap before the age starts.
        let tag_end: Vec<usize> = rows
            .iter()
            .zip(["novacode", "a-very", "api"])
            .map(|(l, tag)| {
                let c: Vec<char> = l.chars().collect();
                assert_eq!(c.len(), panel_w, "the age ends at the right edge: {l:?}");
                let age = c.iter().rposition(|ch| *ch == ' ').unwrap();
                assert!(c[age - 1] == ' ', "a 2-cell gap before the age: {l:?}");
                assert!(l.contains(tag), "tag {tag} pushed out: {l:?}");
                c[..age].iter().rposition(|ch| *ch != ' ').unwrap() + 1
            })
            .collect();
        assert!(
            tag_end.iter().all(|&x| x == tag_end[0]),
            "tags share a right edge {tag_end:?}: {rows:#?}"
        );
        assert!(
            !rows[1].contains("a-very-long-project-name"),
            "the tag is capped: {rows:#?}"
        );
        assert!(rows[0].contains("T3-fix-the-"), "the title keeps its room: {rows:#?}");
        if w == 120 {
            assert!(rows[0].contains('…'), "a long title is cut: {rows:#?}");
        }
    }
}

/// The selected row has an accent `▸` pointer right before its status icon and a bold
/// title; no background on any cell, detail lines unmarked. Unfocused, the pointer is
/// dim, the title stays bold and nothing else dims.
#[test]
fn panel_selection_is_a_pointer_and_bold_title() {
    let t = novacode_tui::theme::Theme::default();
    let (accent, dim, sel) = (t.accent.fg.unwrap(), t.dim.fg.unwrap(), t.selection.bg.unwrap());
    let (mut a, _rx) = app(vec![]);
    board(&mut a);
    a.set_focus(Focus::Panel);
    key(&mut a, KeyCode::Down, KeyModifiers::NONE);
    let x0 = 120 - a.panel.width(120) + 2;
    for focused in [true, false] {
        if !focused {
            key(&mut a, KeyCode::Esc, KeyModifiers::NONE);
            assert_eq!(a.focus(), Focus::Input);
        }
        let buf = draw(&mut a, 120, 24);
        let y = row_of(&text(&buf), "T4-docs");
        // Title, then its 2-cell status icon column, then the pointer.
        let tx = (x0..120).find(|&x| buf[(x, y)].symbol() == "T").unwrap();
        let px = tx - 3;
        assert_eq!(
            buf[(px, y)].symbol(),
            "▸",
            "pointer before the icon (focused {focused})"
        );
        assert_ne!(buf[(px + 1, y)].symbol(), " ", "the icon follows the pointer");
        let want = if focused { accent } else { dim };
        assert_eq!(buf[(px, y)].fg, want, "pointer colour (focused {focused})");
        assert!(
            buf[(tx, y)].modifier.contains(ratatui::style::Modifier::BOLD),
            "the selected title is bold (focused {focused})"
        );
        assert_ne!(buf[(tx, y)].fg, dim, "the title is not dimmed (focused {focused})");
        for yy in [y, y + 1] {
            assert!((x0..120).all(|x| buf[(x, yy)].bg != sel), "no background on line {yy}");
        }
        assert!(
            (x0..120).all(|x| buf[(x, y + 1)].symbol() != "▸"),
            "the detail line is not marked"
        );
        let other = row_of(&text(&buf), "T5-api");
        assert_eq!(buf[(px, other)].symbol(), "─", "only the selected row");
        let ox = (x0..120).find(|&x| buf[(x, other)].symbol() == "T").unwrap();
        assert_eq!(ox, tx, "the pointer column is reserved on every row");
    }
}

/// TP4: ↑↓ move the selection on every row tab (also one without actions), ←→ switch
/// tabs, 1–9 still jump but are not printed, and the footer has one format.
#[test]
fn panel_keys_are_the_same_on_every_tab() {
    let (mut a, _rx) = app(vec![]);
    a.on_agent(AgentEvent::AgentStart);
    a.on_agent(AgentEvent::ToolStart {
        call: ToolCall {
            id: "e".into(),
            name: "edit".into(),
            arguments: json!({"path": "src/a.rs"}),
            arguments_error: None,
            replay: None,
        },
    });
    a.on_agent(AgentEvent::Message(Message::ToolResult {
        call_id: "t".into(),
        tool_name: "todo".into(),
        content: vec![],
        is_error: false,
        details: Some(json!({"todo": [
            {"id": 1, "text": "read the code", "status": "done"},
            {"id": 2, "text": "write the panel", "status": "active"},
        ]})),
    }));
    a.panel.visible = true;
    a.panel.show_tab("agent");
    a.set_focus(Focus::Panel);
    let selected = |a: &mut App, s: &str| {
        let t = text(&draw(a, 120, 24));
        t.lines().find(|l| l.contains(s)).is_some_and(|l| l.contains('▸'))
    };
    assert!(selected(&mut a, "read the code"), "the first row starts selected");
    key(&mut a, KeyCode::Down, KeyModifiers::NONE);
    assert!(selected(&mut a, "write the panel"), "↓ moves on a tab without actions");
    assert!(!selected(&mut a, "read the code"));
    key(&mut a, KeyCode::Up, KeyModifiers::NONE);
    assert!(selected(&mut a, "read the code"), "↑ moves back");
    let s = screen(&mut a, 120, 24);
    assert!(
        !s.contains("1 Agent") && !s.contains("1-9"),
        "numbers are not printed:\n{s}"
    );
    assert!(s.contains("↑↓ move · esc back · ←→ tab"), "one footer format:\n{s}");
    key(&mut a, KeyCode::Right, KeyModifiers::NONE);
    assert_eq!(a.panel.shown.as_deref(), Some("files"), "→ next tab");
    assert!(selected(&mut a, "src/a.rs"), "a fresh tab selects its first row");
    key(&mut a, KeyCode::Left, KeyModifiers::NONE);
    assert_eq!(a.panel.shown.as_deref(), Some("agent"), "← previous tab");
    key(&mut a, KeyCode::Char('2'), KeyModifiers::NONE);
    assert_eq!(a.panel.shown.as_deref(), Some("files"), "2 still jumps");
    assert_eq!(a.focus(), Focus::Panel);
}

/// TP4: Usage and Timing have no row actions: ↑↓ scroll them, with no selection bar.
#[test]
fn usage_and_timing_tabs_scroll() {
    let t = novacode_tui::theme::Theme::default();
    let sel = t.selection.bg.unwrap();
    let (mut a, _rx) = app(vec![]);
    for _ in 0..8 {
        a.on_agent(AgentEvent::AgentStart);
        let mut m = assistant(vec![Block::text("ok")], StopReason::Stop);
        if let Message::Assistant(x) = &mut m {
            x.usage = Usage {
                input: 1000,
                output: 200,
                cache_read: 3000,
                cache_write: 100,
                reasoning: None,
            };
        }
        a.on_agent(AgentEvent::Message(m));
        a.on_agent(AgentEvent::Settled);
    }
    for (tab, top) in [("timing", "per task"), ("usage", "CONTEXT")] {
        a.panel.visible = true;
        a.panel.show_tab(tab);
        a.set_focus(Focus::Panel);
        let x0 = 120 - a.panel.width(120) + 2;
        let s = screen(&mut a, 120, 12);
        assert!(s.contains(top), "{tab} starts at the top:\n{s}");
        key(&mut a, KeyCode::Down, KeyModifiers::NONE);
        let buf = draw(&mut a, 120, 12);
        let s = screen(&mut a, 120, 12);
        assert!(!s.contains(top), "↓ scrolls {tab}:\n{s}");
        assert!(s.contains("↑↓ scroll"), "{tab} footer:\n{s}");
        let bar = (0..12).any(|y| (x0..120).any(|x| buf[(x, y)].bg == sel));
        assert!(!bar, "{tab} draws no selection");
        key(&mut a, KeyCode::Up, KeyModifiers::NONE);
        assert!(screen(&mut a, 120, 12).contains(top), "↑ scrolls {tab} back");
    }
}

/// TP2 baseline: an ask form whose three choices each have a description.
#[test]
fn golden_ask_form() {
    goldens("ask_form", 24, open_ask);
}

/// An ask form with three described choices.
fn open_ask(a: &mut App) {
    std::mem::forget(ask_db(a, 1));
}

/// The database question `n` times; the receiver gets the answers.
fn ask_db(a: &mut App, n: usize) -> tokio::sync::oneshot::Receiver<Option<Vec<novacode_types::Reply>>> {
    let (reply, rx) = tokio::sync::oneshot::channel();
    let choice = |label: &str, description: &str, recommended| Choice {
        label: label.into(),
        description: Some(description.into()),
        preview: None,
        recommended,
    };
    let q = Question {
        question: "Which database should the service use?".into(),
        header: Some("Database".into()),
        options: vec![
            choice("Postgres", "Relational, already run in production.", true),
            choice("SQLite", "One file, no server; fine for one writer.", false),
            choice("Redis", "In memory; fast, but data needs care.", false),
        ],
        multi: false,
    };
    a.on_ui(UiEvent::Ask(novacode_tui::ask::AskReq {
        id: 1,
        questions: vec![q; n],
        reply,
    }));
    rx
}

/// The screen row holding `text` (first match).
fn row_of(s: &str, text: &str) -> u16 {
    s.lines()
        .position(|l| l.contains(text))
        .unwrap_or_else(|| panic!("no {text}: {s}")) as u16
}

/// TP5: every choice shows its description, free text is the last numbered choice,
/// the note hint sits on the selected choice only, and "back" needs an earlier question.
#[test]
fn ask_form_lists_every_choice_and_only_working_keys() {
    let (mut a, _rx) = app(vec![]);
    let _answers = ask_db(&mut a, 2);
    let s = screen(&mut a, 80, 30);
    for d in ["Relational, already", "One file, no server", "In memory; fast"] {
        assert!(s.contains(d), "description {d}: {s}");
    }
    assert!(s.contains("4. Type something…"), "{s}");
    let notes: Vec<&str> = s.lines().filter(|l| l.contains("n note")).collect();
    assert_eq!(notes.len(), 1, "{s}");
    assert!(notes[0].contains("▸ 1. Postgres"), "{s}");
    let keys = overlay_keys(&s);
    assert!(keys.contains("enter choose") && keys.contains("esc dismiss"), "{keys}");
    assert!(!keys.contains("back"), "no earlier question: {keys}");
    // On the free-text row: no note, and Enter types.
    for _ in 0..3 {
        key(&mut a, KeyCode::Down, KeyModifiers::NONE);
    }
    let s = screen(&mut a, 80, 30);
    assert!(s.contains("▸ 4. Type something…") && !s.contains("n note"), "{s}");
    assert!(overlay_keys(&s).contains("enter type"), "{s}");
    // The second question can go back.
    key(&mut a, KeyCode::Up, KeyModifiers::NONE);
    screen(&mut a, 80, 30);
    key(&mut a, KeyCode::Enter, KeyModifiers::NONE);
    let s = screen(&mut a, 80, 30);
    assert!(s.contains("Question 2 of 2"), "{s}");
    assert!(overlay_keys(&s).contains("← back"), "{s}");
}

/// TP5: a click selects a choice, a double-click answers with it.
#[test]
fn ask_form_click_selects_and_double_click_confirms() {
    let (mut a, _rx) = app(vec![]);
    let mut answers = ask_db(&mut a, 1);
    let s = screen(&mut a, 80, 30);
    let sqlite = row_of(&s, "2. SQLite");
    click(&mut a, 10, sqlite);
    let s = screen(&mut a, 80, 30);
    assert!(s.contains("▸ 2. SQLite"), "{s}");
    assert!(answers.try_recv().is_err(), "one click only selects");
    // A click on a description line picks its choice too.
    click(&mut a, 10, row_of(&s, "In memory; fast"));
    assert!(screen(&mut a, 80, 30).contains("▸ 3. Redis"));
    click(&mut a, 10, sqlite);
    click(&mut a, 10, sqlite);
    let r = answers.try_recv().unwrap().unwrap();
    assert_eq!(r[0].chosen, ["SQLite"]);
    // A double-click on free text opens it for typing instead of answering.
    let mut answers = ask_db(&mut a, 1);
    let s = screen(&mut a, 80, 30);
    let free = row_of(&s, "4. Type something…");
    click(&mut a, 10, free);
    click(&mut a, 10, free);
    assert!(answers.try_recv().is_err());
    screen(&mut a, 80, 30);
    typ(&mut a, "mysql");
    key(&mut a, KeyCode::Enter, KeyModifiers::NONE);
    let r = answers.try_recv().unwrap().unwrap();
    assert_eq!(r[0].text.as_deref(), Some("mysql"));
}

/// TP2 baseline: the approval card for a risky command.
#[test]
fn golden_approval_card() {
    goldens("approval_card", 24, |a| {
        a.on_agent(AgentEvent::AgentStart);
        a.on_agent(AgentEvent::Message(Message::user("clean")));
        let (tx, rx) = tokio::sync::oneshot::channel();
        std::mem::forget(rx);
        a.on_ui(UiEvent::Approve(ApproveReq {
            id: 1,
            tool: "bash".into(),
            detail: "rm -rf target".into(),
            reason: "rm -r deletes files".into(),
            rule: "rm -rf target".into(),
            risky: true,
            reply: tx,
        }));
    });
}

/// L9 `/kill`: in a session's own window it is refused with a pointer to `/quit`.
#[test]
fn kill_is_refused_in_the_sessions_own_window() {
    let (mut a, mut rx) = app(vec![]);
    typ(&mut a, "/kill");
    key(&mut a, KeyCode::Enter, KeyModifiers::NONE);
    assert!(rx.try_recv().is_err());
    assert_eq!(a.exit, None);
    let s = screen(&mut a, 120, 12);
    assert!(
        s.contains("/kill works in attached windows") && s.contains("/quit"),
        "{s}"
    );
}

/// L9 `/kill` in an attached window: a card names the session; no sends nothing, yes
/// sends `Kill`.
#[test]
fn kill_asks_then_ends_the_attached_session() {
    let (mut a, mut rx) = app_at(vec![], Some("01abcdef0123456789"));
    typ(&mut a, "/kill");
    key(&mut a, KeyCode::Enter, KeyModifiers::NONE);
    let s = screen(&mut a, 120, 16);
    assert!(s.contains("End session 01abcdef0123?"), "{s}");
    assert!(rx.try_recv().is_err(), "nothing before the answer");
    // Cancel (the second choice) keeps it.
    key(&mut a, KeyCode::Down, KeyModifiers::NONE);
    key(&mut a, KeyCode::Enter, KeyModifiers::NONE);
    assert!(rx.try_recv().is_err(), "cancel sends nothing");
    typ(&mut a, "/kill");
    key(&mut a, KeyCode::Enter, KeyModifiers::NONE);
    key(&mut a, KeyCode::Enter, KeyModifiers::NONE);
    assert_eq!(rx.try_recv().unwrap(), Cmd::Kill);
}

/// L9 `/kill` in super's window (a board came in) names super's root and says tasks keep running.
#[test]
fn kill_in_supers_window_names_the_root() {
    let (mut a, _rx) = app_at(vec![], Some("01abcdef0123456789"));
    a.on_ui(UiEvent::Board {
        tasks: vec![],
        needs: vec![],
    });
    typ(&mut a, "/kill");
    key(&mut a, KeyCode::Enter, KeyModifiers::NONE);
    let s = screen(&mut a, 120, 16);
    assert!(s.contains("End super for /home/u/proj? Tasks keep running."), "{s}");
}

/// L9.19: an idle compaction shows in the activity line while it runs, then
/// leaves a full-width divider and the `compacted ×N` status segment.
#[test]
fn compaction_shows_while_running_then_leaves_a_divider() {
    let (mut a, _rx) = app(vec![Message::user("go")]);
    let s = screen(&mut a, 100, 12);
    assert!(!s.contains("compacted ×"), "{s}");
    a.on_agent(AgentEvent::Compacting { tokens_before: 182_000 });
    let s = screen(&mut a, 100, 12);
    assert!(s.contains("compacting context"), "{s}");
    a.on_agent(AgentEvent::Compacted {
        tokens_before: 182_000,
        tokens_after: 41_000,
        notes: 37,
        fallback: false,
    });
    let s = screen(&mut a, 100, 12);
    assert!(!s.contains("compacting context") && s.contains("ready"), "{s}");
    assert!(s.contains("compacted ×1"), "{s}");
    let line = s
        .lines()
        .find(|l| l.contains("context compacted"))
        .unwrap_or_else(|| panic!("{s}"));
    // The transcript's margins aside, the line runs across the screen.
    assert!(
        line.trim_start()
            .starts_with("━━ context compacted · 182k → 41k tokens · 37 notes ━"),
        "{line}"
    );
    assert!(line.chars().count() >= 97 && line.ends_with('━'), "{line}");
}

fn say_msgs(id: &str, kind: &str, at_ms: u64) -> [Message; 2] {
    let say = Say {
        kind: kind.into(),
        priority: "normal".into(),
        at_ms,
        time: "14:05".into(),
        trigger: "health check".into(),
        body: format!("body {id}"),
        ..Default::default()
    };
    [
        Message::Assistant(AssistantMessage {
            content: vec![Block::ToolCall(ToolCall {
                id: id.into(),
                name: "say".into(),
                arguments: json!({"kind": kind, "body": format!("body {id}")}),
                arguments_error: None,
                replay: None,
            })],
            stop: StopReason::ToolUse,
            usage: Default::default(),
            provider: "f".into(),
            model: "m".into(),
            response_id: None,
            error_message: None,
            thinking_level: None,
        }),
        Message::ToolResult {
            call_id: id.into(),
            tool_name: "say".into(),
            content: vec![Block::text("Shown to the user.")],
            is_error: false,
            details: Some(json!({ "say": say })),
        },
    ]
}

/// L10.9: the "new since you last looked" divider goes before the first `say` block that
/// came while the terminal was unfocused, and on attach before the first one newer than
/// the last time this user's window saw the session. There is only one.
#[test]
fn the_new_since_divider_marks_the_first_unseen_message() {
    let order = |s: &str| {
        let at = |p: &str| s.find(p).unwrap_or_else(|| panic!("{p} missing:\n{s}"));
        (
            at("body c1"),
            at("new since you last looked"),
            at("body c2"),
            at("body c3"),
        )
    };
    // Reattach: c1 seen, c2 and c3 are new.
    let mut history = vec![Message::user("go")];
    history.extend(say_msgs("c1", "update", 1000));
    history.extend(say_msgs("c2", "warning", 3000));
    history.extend(say_msgs("c3", "done", 4000));
    let (mut a, _rx) = app(history);
    a.on_ui(UiEvent::Seen { ms: 2000 });
    let s = screen(&mut a, 100, 40);
    let (c1, div, c2, c3) = order(&s);
    assert!(c1 < div && div < c2 && c2 < c3, "{s}");
    // Live: focused, no divider; unfocused, before the first block of that time away.
    let (mut a, _rx) = app(vec![Message::user("go")]);
    a.on_event(Event::FocusGained);
    for m in say_msgs("c1", "update", 1) {
        a.on_agent(AgentEvent::Message(m));
    }
    assert!(!screen(&mut a, 100, 40).contains("new since"));
    a.on_event(Event::FocusLost);
    for m in say_msgs("c2", "update", 2).into_iter().chain(say_msgs("c3", "note", 3)) {
        a.on_agent(AgentEvent::Message(m));
    }
    a.on_event(Event::FocusGained);
    let s = screen(&mut a, 100, 40);
    let (c1, div, c2, c3) = order(&s);
    assert!(c1 < div && div < c2 && c2 < c3, "{s}");
    assert_eq!(s.matches("new since you last looked").count(), 1);
    // Away again: the divider moves to the new first unseen block.
    a.on_event(Event::FocusLost);
    for m in say_msgs("c4", "update", 4) {
        a.on_agent(AgentEvent::Message(m));
    }
    let s = screen(&mut a, 100, 40);
    assert_eq!(s.matches("new since you last looked").count(), 1);
    assert!(s.find("new since").unwrap() > s.find("body c3").unwrap(), "{s}");
}

/// S1 (live test): a terminal never reports focus until it changes, so a window opened or
/// reconnected in an unfocused tmux window got no FocusLost and never drew the divider.
/// Until the user shows they are there (focus in, a key, a click), the window counts as away.
#[test]
fn a_window_counts_as_away_until_the_user_shows_up() {
    let (mut a, _rx) = app(vec![Message::user("go")]);
    for m in say_msgs("c1", "update", 1) {
        a.on_agent(AgentEvent::Message(m));
    }
    let s = screen(&mut a, 100, 40);
    assert!(
        s.find("new since").is_some_and(|d| d < s.find("body c1").unwrap()),
        "{s}"
    );
    // A key press shows the user is looking: no divider for what comes next.
    let (mut a, _rx) = app(vec![Message::user("go")]);
    a.on_event(Event::Key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE)));
    for m in say_msgs("c1", "update", 1) {
        a.on_agent(AgentEvent::Message(m));
    }
    assert!(!screen(&mut a, 100, 40).contains("new since"));
}

/// L10.9: open questions and warnings head the Tasks tab as Needs you; `d` dismisses one
/// through the host (`/dismiss <id>`), Enter does nothing there.
#[test]
fn needs_you_heads_the_tasks_tab_and_d_dismisses() {
    let (mut a, mut rx) = app(vec![]);
    a.on_ui(UiEvent::Board {
        tasks: vec![novacode_tui::panel::TaskRow {
            label: "T2-api".into(),
            state: "blocked".into(),
            ..Default::default()
        }],
        needs: vec![novacode_tui::panel::NeedRow {
            id: "say-1".into(),
            kind: "warning".into(),
            tasks: vec!["T2-api".into()],
            status: "T2-api is stuck on the db url".into(),
            at_ms: 1,
        }],
    });
    a.panel.visible = true;
    a.panel.show_tab("tasks");
    a.set_focus(Focus::Panel);
    let s = screen(&mut a, 120, 30);
    let (needs, warn, task) = (
        s.find("NEEDS YOU").unwrap_or_else(|| panic!("{s}")),
        s.find("warning · T2-api").unwrap_or_else(|| panic!("{s}")),
        s.rfind("T2-api").unwrap(),
    );
    assert!(needs < warn && warn < task, "{s}");
    while rx.try_recv().is_ok() {}
    a.on_event(Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)));
    assert!(rx.try_recv().is_err(), "Enter dismisses nothing");
    a.on_event(Event::Key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE)));
    let mut sent = vec![];
    while let Ok(c) = rx.try_recv() {
        sent.push(c);
    }
    assert!(sent.contains(&Cmd::Prompt("/dismiss say-1".into())), "{sent:?}");
}

fn screen(app: &mut App, w: u16, h: u16) -> String {
    text(&draw(app, w, h))
}

/// The startup header's version, so a release bump leaves goldens and snapshots alone.
fn unversioned(s: &str) -> String {
    s.replace(concat!("novacode ", env!("CARGO_PKG_VERSION")), "novacode X.Y.Z")
}

fn snapshot(name: &str, got: &str) {
    let got = unversioned(got);
    let got = got.as_str();
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
    ev(
        app,
        AgentEvent::AgentEnd {
            stop: StopReason::Stop,
            prompts: vec![],
        },
    );
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
fn older_sessions_fill_prompt_history() {
    let (mut a, mut rx) = app(vec![Message::user("mine")]);
    a.on_ui(UiEvent::History {
        prompts: vec!["old one".into(), "old two".into()],
    });
    // This session's prompts are newest; the folder's older ones come after them on Up.
    key(&mut a, KeyCode::Up, KeyModifiers::NONE);
    assert_eq!(a.editor_text(), "mine");
    key(&mut a, KeyCode::Up, KeyModifiers::NONE);
    key(&mut a, KeyCode::Enter, KeyModifiers::NONE);
    assert_eq!(rx.try_recv().unwrap(), Cmd::Prompt("old two".into()));
}

#[test]
fn esc_puts_queued_follow_ups_back() {
    let (mut a, mut rx) = app(vec![]);
    a.on_agent(AgentEvent::AgentStart);
    for (text, m) in [("use tests", KeyModifiers::NONE), ("then docs", KeyModifiers::ALT)] {
        for c in text.chars() {
            key(&mut a, KeyCode::Char(c), KeyModifiers::NONE);
        }
        key(&mut a, KeyCode::Enter, m);
    }
    while rx.try_recv().is_ok() {}
    key(&mut a, KeyCode::Esc, KeyModifiers::NONE);
    assert_eq!(rx.try_recv().unwrap(), Cmd::TakeFollowUps);
    assert_eq!(rx.try_recv().unwrap(), Cmd::Abort);
    // The host took back the queued follow-up, plus one an extension queued: both return to the editor.
    a.on_ui(UiEvent::FollowUpsBack {
        texts: vec!["then docs".into(), "from an extension".into()],
    });
    assert_eq!(a.editor_text(), "then docs\nfrom an extension");
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

/// The status line: the screen's last row.
fn status(a: &mut App) -> String {
    screen(a, 100, 10).lines().last().unwrap().to_string()
}

fn usage_out(output: u64) -> AgentEvent {
    AgentEvent::Stream(StreamEvent::Usage(Usage {
        output,
        ..Default::default()
    }))
}

/// G1: while busy, elapsed time and the interrupt key always show; tokens only once
/// the provider reports usage for this turn.
#[test]
fn activity_line_while_busy_without_usage() {
    let (mut a, _rx) = app(vec![]);
    a.on_agent(AgentEvent::AgentStart);
    let s = status(&mut a);
    assert!(
        s.contains("working") && s.contains("0.0s") && s.contains("esc interrupt"),
        "{s}"
    );
    assert!(!s.contains("tokens"), "{s}");
    a.on_agent(AgentEvent::Settled);
    let s = status(&mut a);
    assert!(!s.contains("interrupt") && !s.contains("0.0s"), "{s}");
}

#[test]
fn activity_line_counts_the_turns_output_tokens() {
    let (mut a, _rx) = app(vec![]);
    a.on_agent(AgentEvent::AgentStart);
    a.on_agent(usage_out(700));
    assert!(
        status(&mut a).contains("0.0s · 700 tokens · esc interrupt"),
        "{}",
        status(&mut a)
    );
    // The finished reply's usage (500 output) replaces its live count; the next reply adds.
    a.on_agent(AgentEvent::Message(assistant(
        vec![Block::text("hi")],
        StopReason::ToolUse,
    )));
    assert!(status(&mut a).contains("500 tokens"), "{}", status(&mut a));
    a.on_agent(usage_out(1000));
    assert!(status(&mut a).contains("1.5k tokens"), "{}", status(&mut a));
    // A new turn starts counting again.
    a.on_agent(AgentEvent::Settled);
    a.on_agent(AgentEvent::AgentStart);
    assert!(!status(&mut a).contains("tokens"), "{}", status(&mut a));
}

#[test]
fn activity_line_names_a_rebound_interrupt_key() {
    let mut i = init(vec![], None);
    let over = [("app.interrupt".to_string(), json!("ctrl+g"))].into_iter().collect();
    i.keymap = novacode_tui::keys::Keymap::new(&over).0;
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut a = App::new(i, tx);
    a.on_agent(AgentEvent::AgentStart);
    let s = status(&mut a);
    assert!(s.contains("ctrl+g interrupt") && !s.contains("esc"), "{s}");
}

/// Every fold line says what it hides: a tool's extension rows past the limit too.
#[test]
fn tool_ui_rows_past_the_limit_get_a_fold_line() {
    let (mut a, _rx) = app(vec![]);
    a.on_agent(AgentEvent::Message(Message::user("go")));
    a.on_agent(AgentEvent::Message(assistant(
        vec![call("x", "status", json!({}))],
        StopReason::ToolUse,
    )));
    let rows: Vec<String> = (1..=15).map(|i| format!("row {i}")).collect();
    a.on_agent(AgentEvent::Message(Message::ToolResult {
        call_id: "x".into(),
        tool_name: "status".into(),
        content: vec![Block::text("ok")],
        is_error: false,
        details: Some(json!({ "ui": rows })),
    }));
    let s = screen(&mut a, 60, 30);
    assert!(s.contains("row 12") && !s.contains("row 13"), "{s}");
    assert!(s.contains("… (3 more lines) · ctrl+o expand"), "{s}");
}

fn key(a: &mut App, code: KeyCode, m: KeyModifiers) {
    a.on_key(KeyEvent::new(code, m));
}

/// TS1: scrolled up, the top screen line stays put while content above it changes
/// height: a tool's output grows, turns fold, and the panel re-wraps the transcript.
#[test]
fn the_top_line_stays_while_content_above_changes() {
    let (mut a, _rx) = app(vec![]);
    let turn = |a: &mut App, n: usize| {
        a.on_agent(AgentEvent::Message(Message::user(format!("prompt {n}"))));
        let out = (1..=30).map(|i| format!("out {n}.{i}")).collect::<Vec<_>>().join("\n");
        let id = format!("t{n}");
        a.on_agent(AgentEvent::Message(assistant(
            vec![call(&id, "bash", json!({"command": "make"}))],
            StopReason::ToolUse,
        )));
        a.on_agent(AgentEvent::Message(result(&id, "bash", &out, false)));
        let long = "a long paragraph that wraps differently at each width ".repeat(8);
        let lines = (1..=40)
            .map(|i| format!("- answer {n}.{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        a.on_agent(AgentEvent::Message(assistant(
            vec![Block::text(format!("{long}\n\n{lines}"))],
            StopReason::Stop,
        )));
        a.on_agent(AgentEvent::Settled);
    };
    turn(&mut a, 1);
    turn(&mut a, 2);
    // Open every turn, then page up into the last answer.
    key(&mut a, KeyCode::Char('o'), KeyModifiers::ALT);
    screen(&mut a, 120, 20);
    key(&mut a, KeyCode::PageUp, KeyModifiers::NONE);
    // The transcript part of the first row (the panel, when open, is on the right).
    let first = |a: &mut App| {
        let w = if a.panel.visible { 120 - a.panel.width(120) } else { 120 };
        let s = screen(a, 120, 20);
        s.lines()
            .next()
            .unwrap()
            .chars()
            .take(w as usize)
            .collect::<String>()
            .trim()
            .to_string()
    };
    let top = first(&mut a);
    assert!(top.contains("answer 2."), "{top}");
    key(&mut a, KeyCode::Char('o'), KeyModifiers::CONTROL);
    assert_eq!(first(&mut a), top, "tool output grew above");
    key(&mut a, KeyCode::Char('o'), KeyModifiers::ALT);
    assert_eq!(first(&mut a), top, "turns folded");
    a.panel.visible = true;
    assert_eq!(first(&mut a), top, "panel opened");
    a.panel.visible = false;
    assert_eq!(first(&mut a), top, "panel closed");
}

/// An app with `slow` set (L9.17: no motion).
fn slow_app() -> (App, mpsc::UnboundedReceiver<Cmd>) {
    let (tx, rx) = mpsc::unbounded_channel();
    let mut i = init(vec![], None);
    i.slow = true;
    (App::new(i, tx), rx)
}

/// A reply of 200 numbered lines.
fn long_reply(a: &mut App) {
    a.on_agent(AgentEvent::Message(Message::user("go")));
    let text = (1..=200).map(|i| format!("- row {i}")).collect::<Vec<_>>().join("\n");
    a.on_agent(AgentEvent::Message(assistant(
        vec![Block::text(text)],
        StopReason::Stop,
    )));
    a.on_agent(AgentEvent::Settled);
}

/// The first text row of a frame drawn mid-motion.
fn top_row(a: &mut App) -> String {
    let s = text(&draw_live(a, 80, 20));
    s.lines().find(|l| !l.trim().is_empty()).unwrap().trim().to_string()
}

const FRAME: std::time::Duration = std::time::Duration::from_millis(16);

/// TS4: PgUp and the wheel ease to their target over 4 frames; slow mode jumps at once;
/// a new scroll during the motion jumps straight to its target.
#[test]
fn scrolling_eases_to_its_target() {
    let (mut a, _rx) = app(vec![]);
    let (mut s, _srx) = slow_app();
    long_reply(&mut a);
    long_reply(&mut s);
    let start = top_row(&mut a);
    assert_eq!(top_row(&mut s), start);
    key(&mut a, KeyCode::PageUp, KeyModifiers::NONE);
    key(&mut s, KeyCode::PageUp, KeyModifiers::NONE);
    let want = top_row(&mut s);
    assert_ne!(want, start, "slow mode jumps at once");
    assert!(!s.moving(), "slow mode never animates");
    assert!(a.moving(), "a redraw is asked for while moving");
    let mut frames = vec![top_row(&mut a)];
    for _ in 0..3 {
        novacode_tui::clock::advance(FRAME);
        frames.push(top_row(&mut a));
    }
    assert_ne!(frames[0], start, "the first frame already moves");
    assert_ne!(frames[0], want, "{frames:?}");
    assert_ne!(frames[2], want, "{frames:?}");
    assert_eq!(frames[3], want, "the target after 4 frames: {frames:?}");
    assert!(!a.moving(), "no redraws once there");
    // A second scroll mid-motion lands on its target in the next frame.
    key(&mut a, KeyCode::PageUp, KeyModifiers::NONE);
    key(&mut s, KeyCode::PageUp, KeyModifiers::NONE);
    let mid = top_row(&mut a);
    top_row(&mut s);
    key(&mut a, KeyCode::PageUp, KeyModifiers::NONE);
    key(&mut s, KeyCode::PageUp, KeyModifiers::NONE);
    let want = top_row(&mut s);
    assert_ne!(mid, want);
    assert_eq!(top_row(&mut a), want, "no lag builds up");
    assert!(!a.moving());
    // The wheel eases too.
    wheel(&mut a, 10, 5, false);
    wheel(&mut s, 10, 5, false);
    let want = top_row(&mut s);
    assert_ne!(top_row(&mut a), want);
    novacode_tui::clock::advance(FRAME * 3);
    assert_eq!(top_row(&mut a), want);
}

/// A one-question form with `n` short context lines and choices A, B.
fn ask(a: &mut App, n: usize) -> tokio::sync::oneshot::Receiver<Option<Vec<Reply>>> {
    let (reply, answers) = tokio::sync::oneshot::channel();
    let question: String = (1..=n).map(|i| format!("context {i}\n")).collect();
    a.on_ui(UiEvent::Ask(novacode_tui::ask::AskReq {
        id: 1,
        questions: vec![Question {
            question,
            header: None,
            options: ["A", "B"]
                .map(|l| Choice {
                    label: l.into(),
                    description: None,
                    preview: None,
                    recommended: false,
                })
                .to_vec(),
            multi: false,
        }],
        reply,
    }));
    answers
}

/// Rows of the first frame from the top (a card; the input's frame is below it).
fn card_rows(s: &str) -> usize {
    let top = s.lines().position(|l| l.trim_start().starts_with('╭')).unwrap();
    let input = s
        .lines()
        .collect::<Vec<_>>()
        .iter()
        .rposition(|l| l.trim_start().starts_with('╭'))
        .unwrap();
    input - top
}

/// TS4: the ask form and the approval card grow to full height over 3 frames (at once in
/// slow mode), and keys work from the first frame.
#[test]
fn cards_grow_over_three_frames() {
    let (mut a, _rx) = app(vec![]);
    let _answers = ask(&mut a, 3);
    let mut rows = vec![card_rows(&text(&draw_live(&mut a, 80, 30)))];
    for _ in 0..2 {
        novacode_tui::clock::advance(FRAME);
        rows.push(card_rows(&text(&draw_live(&mut a, 80, 30))));
    }
    assert!(rows[0] < rows[1] && rows[1] < rows[2], "{rows:?}");
    assert_eq!(
        rows[2],
        card_rows(&screen(&mut a, 80, 30)),
        "full height after 3 frames"
    );
    assert!(!a.moving());
    // Slow mode: full at once.
    let (mut s, _srx) = slow_app();
    let _sa = ask(&mut s, 3);
    assert_eq!(card_rows(&text(&draw_live(&mut s, 80, 30))), rows[2]);
    // The approval card grows too.
    let (mut a, _rx) = app(vec![]);
    let (tx, _reply) = tokio::sync::oneshot::channel();
    a.on_ui(UiEvent::Approve(ApproveReq {
        id: 1,
        tool: "bash".into(),
        detail: "rm -rf target".into(),
        reason: "rm -r deletes files".into(),
        rule: "rm -rf target".into(),
        risky: true,
        reply: tx,
    }));
    let first = card_rows(&text(&draw_live(&mut a, 80, 30)));
    assert!(first < card_rows(&screen(&mut a, 80, 30)));
}

/// TS4: a key pressed in a card's first frame is handled.
#[test]
fn keys_work_while_a_card_grows() {
    let (mut a, mut rx) = app(vec![]);
    let mut answers = ask(&mut a, 3);
    draw_live(&mut a, 80, 30);
    assert!(a.moving());
    key(&mut a, KeyCode::Char('2'), KeyModifiers::NONE);
    let got = answers.try_recv().expect("answered").expect("not dismissed");
    assert_eq!(got.len(), 1, "{got:?}");
    assert!(format!("{got:?}").contains('B'), "{got:?}");
    let _ = rx.try_recv();
}

/// TS4: an ask form too tall for its spaced layout reserves the rows of the compact one,
/// so no blank row sits above its hint line.
#[test]
fn a_compact_ask_form_reserves_its_own_height() {
    let (mut a, _rx) = app(vec![]);
    // Spaced: 13 + gap + 2 choices + typed + gap + hints = 19 rows, over the 18 inside a
    // 30-row screen's form; compact: 17.
    let _answers = ask(&mut a, 13);
    let s = screen(&mut a, 80, 30);
    assert!(overlay_keys(&s).starts_with("enter choose"), "{s}");
    assert_eq!(card_rows(&s), 17 + 2, "{s}");
}

/// A left click (press and release) at a cell.
fn click(a: &mut App, column: u16, row: u16) {
    for kind in [
        MouseEventKind::Down(MouseButton::Left),
        MouseEventKind::Up(MouseButton::Left),
    ] {
        a.on_event(Event::Mouse(MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }));
    }
}

const FOCUS_PANEL: (KeyCode, KeyModifiers) = (KeyCode::Char('f'), KeyModifiers::CONTROL.union(KeyModifiers::ALT));

/// TP3: a click in the chat takes focus from the panel; a click in the panel gives it back.
#[test]
fn a_click_moves_focus() {
    let (mut a, _rx) = app(vec![]);
    key(&mut a, FOCUS_PANEL.0, FOCUS_PANEL.1);
    screen(&mut a, 120, 24);
    assert_eq!(a.focus(), Focus::Panel);
    click(&mut a, 5, 3);
    assert_eq!(a.focus(), Focus::Input, "a click in the chat");
    screen(&mut a, 120, 24);
    click(&mut a, 110, 5);
    assert_eq!(a.focus(), Focus::Panel, "a click in the panel");
    // The input box is part of the chat too.
    let s = screen(&mut a, 120, 24);
    let input_row = s.lines().position(|l| l.starts_with('╭')).unwrap() as u16;
    click(&mut a, 5, input_row + 1);
    assert_eq!(a.focus(), Focus::Input, "a click in the input");
}

/// TP3: Esc returns focus to the input, from the panel and from a dialog.
#[test]
fn esc_returns_focus_to_the_input() {
    let (mut a, _rx) = app(vec![]);
    key(&mut a, FOCUS_PANEL.0, FOCUS_PANEL.1);
    assert_eq!(a.focus(), Focus::Panel);
    key(&mut a, KeyCode::Esc, KeyModifiers::NONE);
    assert_eq!(a.focus(), Focus::Input, "from the panel");
    key(&mut a, FOCUS_PANEL.0, FOCUS_PANEL.1);
    open_ask(&mut a);
    assert_eq!(a.focus(), Focus::Dialog, "a question is open");
    key(&mut a, KeyCode::Esc, KeyModifiers::NONE);
    assert_eq!(a.focus(), Focus::Input, "from a dialog");
}

/// TP3 style check: exactly one frame is coloured, the focused one. The input keeps
/// its thinking-level colour while focused and turns `border` grey otherwise.
#[test]
fn only_the_focused_frame_is_coloured() {
    let t = novacode_tui::theme::Theme::default();
    let level = t.levels[ThinkingLevel::ALL
        .iter()
        .position(|l| *l == ThinkingLevel::default())
        .unwrap()]
    .fg;
    // (input corner, panel edge, dialog corners) colours.
    let frames = |a: &mut App| {
        let mut term = Terminal::new(TestBackend::new(120, 30)).unwrap();
        term.draw(|f| a.draw(f)).unwrap();
        let buf = term.backend().buffer();
        let corners: Vec<(u16, u16)> = (0..30)
            .flat_map(|y| (0..120).map(move |x| (x, y)))
            .filter(|&(x, y)| buf[(x, y)].symbol() == "╭")
            .collect();
        // The input is the lowest frame on the left edge; an ask card sits above it.
        let input = corners.iter().copied().rev().find(|&(x, _)| x == 0).unwrap();
        let edge = 120 - a.panel.width(120);
        assert_eq!(buf[(edge, 0)].symbol(), "│");
        let dialogs: Vec<_> = corners.iter().filter(|&&c| c != input).map(|&c| buf[c].fg).collect();
        (buf[input].fg, buf[(edge, 0)].fg, dialogs)
    };
    let (mut a, _rx) = app(vec![]);
    key(&mut a, KeyCode::Char('p'), KeyModifiers::CONTROL | KeyModifiers::ALT);
    let grey = t.border.fg.unwrap();
    let accent = t.accent.fg.unwrap();
    assert_eq!(frames(&mut a), (level.unwrap(), grey, vec![]), "input focused");
    key(&mut a, FOCUS_PANEL.0, FOCUS_PANEL.1);
    assert_eq!(frames(&mut a), (grey, accent, vec![]), "panel focused");
    open_ask(&mut a);
    assert_eq!(frames(&mut a), (grey, grey, vec![accent]), "dialog focused");
}

/// R7 (L10.9): a board snapshot as the wire carries it (`ui` `board`) reaches an
/// attached window's Tasks tab, which Ctrl+Alt+P then shows with its row.
#[test]
fn board_snapshot_from_the_wire_fills_the_tasks_tab() {
    let (mut a, _rx) = app(vec![Message::user("go")]);
    let started = novacode_tui::clock::now_ms() - 65_000;
    let wire = format!(
        r#"{{"kind":"board","tasks":[{{"label":"T1-cli-input-errors","repo":"calc","state":"running","model":"claude/claude-haiku-5-5","tokens":0,"started_ms":{started},"session":"01a1a760"}}]}}"#
    );
    a.on_ui(serde_json::from_str::<UiEvent>(&wire).unwrap());
    a.tick();
    key(&mut a, KeyCode::Char('p'), KeyModifiers::CONTROL | KeyModifiers::ALT);
    let s = screen(&mut a, 120, 16);
    assert!(s.contains("Tasks") && s.contains("T1-cli-input-errors"), "{s}");
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
    // Type-ahead right after the card appears answers nothing.
    key(&mut a, KeyCode::Char('a'), KeyModifiers::NONE);
    assert!(reply.try_recv().is_err());
    a.age_approvals();
    // Typing goes to the card, not the editor; Enter never approves.
    key(&mut a, KeyCode::Char('x'), KeyModifiers::NONE);
    key(&mut a, KeyCode::Enter, KeyModifiers::NONE);
    assert!(a.editor.is_empty());
    assert!(reply.try_recv().is_err());
    key(&mut a, KeyCode::Char('a'), KeyModifiers::NONE);
    assert_eq!(reply.try_recv().unwrap(), Answer::Session);
    let s = screen(&mut a, 60, 14);
    assert!(s.contains("allowed for this session: bash rm -rf target"), "{s}");
}

/// TP5: the approval card ignores the mouse; its fixed letters show as hints.
#[test]
fn approval_card_ignores_clicks() {
    let (mut a, _rx) = app(vec![]);
    let (tx, mut reply) = tokio::sync::oneshot::channel();
    a.on_ui(UiEvent::Approve(ApproveReq {
        id: 1,
        tool: "bash".into(),
        detail: "rm -rf target".into(),
        reason: "deletes files".into(),
        rule: "Bash(rm -rf:*)".into(),
        risky: false,
        reply: tx,
    }));
    a.age_approvals();
    let s = screen(&mut a, 80, 24);
    let keys = row_of(&s, "y once");
    assert!(s.contains("y once · n deny · a this session · A always"), "{s}");
    assert!(s.contains("Bash(rm -rf:*)"), "the rule stays visible: {s}");
    assert!(!s.contains("why:"), "{s}");
    let top = row_of(&s, "Allow?");
    for row in top..=keys + 1 {
        for col in [0, 2, 4, 10, 30] {
            click(&mut a, col, row);
            click(&mut a, col, row);
        }
    }
    assert!(reply.try_recv().is_err(), "no click answers");
    assert!(screen(&mut a, 80, 24).contains("Allow?"));
    key(&mut a, KeyCode::Char('y'), KeyModifiers::NONE);
    assert_eq!(reply.try_recv().unwrap(), Answer::Once);
}

#[test]
fn approval_reason_takes_a_paste() {
    let (mut a, _rx) = app(vec![]);
    let (tx, mut reply) = tokio::sync::oneshot::channel();
    a.on_ui(UiEvent::Approve(ApproveReq {
        id: 1,
        tool: "bash".into(),
        detail: "rm -rf target".into(),
        reason: "deletes files".into(),
        rule: "rm -rf target".into(),
        risky: false,
        reply: tx,
    }));
    a.age_approvals();
    key(&mut a, KeyCode::Char('n'), KeyModifiers::NONE);
    a.on_event(Event::Paste("too risky".into()));
    assert!(a.editor.is_empty(), "the paste went to the card");
    key(&mut a, KeyCode::Enter, KeyModifiers::NONE);
    assert_eq!(reply.try_recv().unwrap(), Answer::Deny(Some("too risky".into())));
}

#[test]
fn next_queued_card_gets_its_own_grace() {
    let (mut a, _rx) = app(vec![]);
    let mut replies = Vec::new();
    for id in 1..=2 {
        let (tx, reply) = tokio::sync::oneshot::channel();
        a.on_ui(UiEvent::Approve(ApproveReq {
            id,
            tool: "bash".into(),
            detail: format!("rm -rf t{id}"),
            reason: "deletes files".into(),
            rule: format!("rm -rf t{id}"),
            risky: false,
            reply: tx,
        }));
        replies.push(reply);
    }
    a.age_approvals();
    key(&mut a, KeyCode::Char('y'), KeyModifiers::NONE);
    assert_eq!(replies[0].try_recv().unwrap(), Answer::Once);
    // The second card was queued long ago, but it only just came to the front.
    key(&mut a, KeyCode::Char('a'), KeyModifiers::NONE);
    assert!(replies[1].try_recv().is_err());
    // With no reason box open, a paste goes to the prompt.
    a.on_event(Event::Paste("next task".into()));
    assert_eq!(a.editor.text(), "next task");
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
    let (mut a, mut rx) = app(vec![
        Message::user("cargo test -p core"),
        Message::user("cargo build"),
        Message::user("git status"),
    ]);
    key(&mut a, KeyCode::Char('r'), KeyModifiers::CONTROL);
    for c in "carg".chars() {
        key(&mut a, KeyCode::Char(c), KeyModifiers::NONE);
    }
    assert_eq!(a.editor.text(), "cargo build");
    // Ctrl+R again: the next older match, still searching.
    key(&mut a, KeyCode::Char('r'), KeyModifiers::CONTROL);
    assert!(a.editor.searching());
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

/// TP7 (G2): an empty input hints `? for shortcuts`; `?` there opens the shortcut
/// sheet instead of typing, and the next key only closes it.
#[test]
fn question_mark_opens_the_shortcut_sheet_on_an_empty_input() {
    let (mut a, _rx) = app(vec![]);
    let s = screen(&mut a, 80, 24);
    assert!(s.contains("? for shortcuts"), "{s}");
    typ(&mut a, "?");
    assert_eq!(a.editor.text(), "", "the ? is not typed");
    assert_eq!(a.focus(), Focus::Dialog);
    let s = screen(&mut a, 80, 24);
    // Labels come from the keymap.
    assert!(s.contains("ctrl+l") && s.contains("esc"), "{s}");
    assert!(s.contains("alt+/ all keys"), "{s}");
    key(&mut a, KeyCode::Char('x'), KeyModifiers::NONE);
    assert_eq!(a.editor.text(), "", "the closing key is not typed");
    assert_eq!(a.focus(), Focus::Input);
    assert!(!screen(&mut a, 80, 24).contains("all keys"));
}

/// TP7: with text in the input, `?` is typed like any key and the hint is gone.
#[test]
fn question_mark_types_on_a_non_empty_input() {
    let (mut a, _rx) = app(vec![]);
    typ(&mut a, "why?");
    assert_eq!(a.editor.text(), "why?");
    let s = screen(&mut a, 80, 24);
    assert!(!s.contains("for shortcuts") && !s.contains("all keys"), "{s}");
}

/// TP7 (G2): the shortcut sheet's layout.
#[test]
fn golden_shortcut_sheet() {
    goldens("shortcut_sheet", 24, |a| typ(a, "?"));
}

/// TP7: a click in the input box moves the cursor to the clicked cell.
#[test]
fn a_click_in_the_input_moves_the_cursor() {
    let (mut a, _rx) = app(vec![]);
    typ(&mut a, "hello world");
    key(&mut a, KeyCode::Enter, KeyModifiers::SHIFT);
    typ(&mut a, "second line");
    let s = screen(&mut a, 80, 24);
    let top = s.lines().position(|l| l.starts_with('╭')).unwrap() as u16;
    // Text starts after the border and one cell of padding.
    click(&mut a, 2 + 6, top + 1);
    typ(&mut a, "X");
    assert_eq!(a.editor.text(), "hello Xworld\nsecond line");
    screen(&mut a, 80, 24);
    // Past the end of a row: the row's end.
    click(&mut a, 60, top + 2);
    typ(&mut a, "!");
    assert_eq!(a.editor.text(), "hello Xworld\nsecond line!");
}

/// A wheel step at a cell: `up` scrolls up.
fn wheel(a: &mut App, column: u16, row: u16, up: bool) {
    let kind = if up {
        MouseEventKind::ScrollUp
    } else {
        MouseEventKind::ScrollDown
    };
    a.on_event(Event::Mouse(MouseEvent {
        kind,
        column,
        row,
        modifiers: KeyModifiers::NONE,
    }));
}

/// A dialog's hint line (overlay or ask form): the last row inside its frame (the first
/// frame from the top; the input's is below it).
fn overlay_keys(s: &str) -> String {
    let end = s.lines().position(|l| l.trim_start().starts_with('╰')).unwrap();
    s.lines()
        .nth(end - 1)
        .unwrap()
        .trim_matches(|c| c == '│' || c == ' ')
        .to_string()
}

/// TP8: while the shortcut sheet is open, a click closes it and leaves the cursor alone.
#[test]
fn a_click_closes_the_shortcut_sheet() {
    let (mut a, _rx) = app(vec![]);
    let s = screen(&mut a, 80, 24);
    let top = s.lines().position(|l| l.starts_with('╭')).unwrap() as u16;
    typ(&mut a, "?");
    screen(&mut a, 80, 24);
    click(&mut a, 10, top + 1);
    assert_eq!(a.focus(), Focus::Input);
    assert!(!screen(&mut a, 80, 24).contains("all keys"));
    typ(&mut a, "hi");
    assert_eq!(a.editor.text(), "hi");
}

fn models(a: &mut App) {
    let items = ["opus", "sonnet", "haiku"]
        .map(|m| PickItem {
            label: m.into(),
            detail: String::new(),
            value: format!("claude/{m}"),
        })
        .to_vec();
    a.on_ui(UiEvent::Models { result: Ok(items) });
}

/// TP8: a click on a picker row selects it, a double-click picks it.
#[test]
fn picker_click_selects_and_double_click_picks() {
    let (mut a, mut rx) = app(vec![]);
    models(&mut a);
    let s = screen(&mut a, 80, 24);
    let row = row_of(&s, "sonnet");
    let col = s.lines().nth(row as usize).unwrap().find("sonnet").unwrap() as u16;
    click(&mut a, col, row);
    assert!(screen(&mut a, 80, 24).contains("▸ sonnet"));
    assert!(rx.try_recv().is_err(), "one click only selects");
    click(&mut a, col, row);
    assert_eq!(rx.try_recv().unwrap(), Cmd::SetModel("claude/sonnet".into()));
    assert_eq!(a.focus(), Focus::Input, "the picker closed");
}

/// TP8: the wheel scrolls the pager, the process view and the side chat.
#[test]
fn the_wheel_scrolls_overlays() {
    let text: String = (1..=80).map(|i| format!("line {i}\n")).collect();
    // Pager: starts at the top; down shows later lines, up goes back.
    let (mut a, _rx) = app(vec![]);
    a.on_ui(UiEvent::Page {
        title: "Output".into(),
        text: text.clone(),
    });
    let s = screen(&mut a, 80, 24);
    assert!(s.contains("line 1\n") || s.contains("line 1 "), "{s}");
    wheel(&mut a, 40, 10, false);
    let s = screen(&mut a, 80, 24);
    assert!(!s.contains("line 2 ") && s.contains("line 4"), "{s}");
    wheel(&mut a, 40, 10, true);
    assert!(screen(&mut a, 80, 24).contains("line 2 "));
    // Process view: follows the tail; up shows older output.
    let (mut a, _rx) = app(vec![]);
    typ(&mut a, "/ps");
    key(&mut a, KeyCode::Enter, KeyModifiers::NONE);
    a.on_ui(UiEvent::Procs {
        items: vec![novacode_tui::procs::ProcView {
            id: "p1".into(),
            name: "dev".into(),
            command: "npm run dev".into(),
            status: "running".into(),
            state: "running".into(),
            started_ms: 1,
            output: text.clone(),
        }],
    });
    let s = screen(&mut a, 80, 24);
    assert!(s.contains("line 80"), "{s}");
    wheel(&mut a, 40, 10, true);
    assert!(!screen(&mut a, 80, 24).contains("line 80"));
    wheel(&mut a, 40, 10, false);
    assert!(screen(&mut a, 80, 24).contains("line 80"));
    // Side chat: follows the newest; up shows older rows.
    let (mut a, _rx) = app(vec![]);
    typ(&mut a, "/btw why");
    key(&mut a, KeyCode::Enter, KeyModifiers::NONE);
    a.on_ui(UiEvent::Btw {
        result: Ok(text.replace('\n', "\n\n")),
    });
    let s = screen(&mut a, 80, 24);
    assert!(s.contains("line 80"), "{s}");
    wheel(&mut a, 40, 10, true);
    assert!(!screen(&mut a, 80, 24).contains("line 80"));
}

/// TP8: every overlay ends with a hint line in the shared `key action · key action` format.
#[test]
fn every_overlay_shows_hints() {
    let (mut a, _rx) = app(vec![]);
    models(&mut a);
    assert_eq!(
        overlay_keys(&screen(&mut a, 80, 24)),
        "↑↓ move · enter pick · esc cancel"
    );
    key(&mut a, KeyCode::Esc, KeyModifiers::NONE);
    a.on_ui(UiEvent::Page {
        title: "Output".into(),
        text: "x".into(),
    });
    assert!(overlay_keys(&screen(&mut a, 80, 24)).starts_with("↑↓ scroll · "));
    key(&mut a, KeyCode::Esc, KeyModifiers::NONE);
    key(&mut a, KeyCode::Char('/'), KeyModifiers::ALT);
    let s = screen(&mut a, 80, 24);
    assert!(overlay_keys(&s).contains("type filter"), "{s}");
    assert!(!s.contains("Esc closes"), "{s}");
    key(&mut a, KeyCode::Esc, KeyModifiers::NONE);
    typ(&mut a, "/ps");
    key(&mut a, KeyCode::Enter, KeyModifiers::NONE);
    assert!(overlay_keys(&screen(&mut a, 80, 24)).starts_with("←→ process · "));
    key(&mut a, KeyCode::Esc, KeyModifiers::NONE);
    typ(&mut a, "/btw");
    key(&mut a, KeyCode::Enter, KeyModifiers::NONE);
    let s = screen(&mut a, 80, 24);
    assert!(overlay_keys(&s).starts_with("enter ask · "), "{s}");
    assert!(!s.contains("PgUp/PgDn scroll"), "{s}");
    key(&mut a, KeyCode::Esc, KeyModifiers::NONE);
    a.on_ui(UiEvent::LoginStep {
        provider: "claude".into(),
        step: novacode_tui::login::LoginStep::Url {
            url: "https://claude.ai/x".into(),
            note: "Sign in".into(),
            paste: true,
        },
    });
    let s = screen(&mut a, 80, 24);
    let keys = overlay_keys(&s);
    assert!(
        keys.ends_with("copy URL · enter submit · esc cancel · ctrl+o open"),
        "{s}"
    );
}

/// TP8 (story 27): the wheel scrolls an ask form that does not fit.
#[test]
fn the_wheel_scrolls_a_long_ask_form() {
    let (mut a, _rx) = app(vec![]);
    let (reply, _answers) = tokio::sync::oneshot::channel();
    let question: String = (1..=30).map(|i| format!("context {i}\n")).collect();
    a.on_ui(UiEvent::Ask(novacode_tui::ask::AskReq {
        id: 1,
        questions: vec![Question {
            question,
            header: None,
            options: vec![Choice {
                label: "Go".into(),
                description: None,
                preview: None,
                recommended: false,
            }],
            multi: false,
        }],
        reply,
    }));
    let s = screen(&mut a, 80, 30);
    assert!(s.contains("context 1\n") || s.contains("context 1 "), "{s}");
    let row = row_of(&s, "context 2");
    wheel(&mut a, 10, row, false);
    let s = screen(&mut a, 80, 30);
    assert!(!s.contains("context 2 ") && s.contains("context 4"), "{s}");
    wheel(&mut a, 10, row, true);
    assert!(screen(&mut a, 80, 30).contains("context 2 "));
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
    let d = tempfile_dir("real");
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
            compactions: 0,
            commands: vec![],
            draft: String::new(),
            attached: None,
            statusline: Default::default(),
            panel_only: false,
            panel_auto: false,
            nerd_icons: false,
            slow: false,
            images: "off".into(),
            procs: Vec::new(),
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

#[test]
fn completion_after_multibyte_space() {
    let d = tempfile_dir("nbsp");
    std::fs::write(d.join("xfile.rs"), "").unwrap();
    let (mut a, _rx) = app(vec![]);
    a.on_agent(AgentEvent::CwdChanged {
        cwd: d.display().to_string(),
        branch: None,
    });
    // Typed key by key: the completion runs after each one.
    typ(&mut a, "a\u{a0}@x");
    assert!(screen(&mut a, 60, 12).contains("▸ xfile.rs"));
    key(&mut a, KeyCode::Tab, KeyModifiers::NONE);
    assert_eq!(a.editor.text(), "a\u{a0}@xfile.rs ");
}

#[test]
fn file_list_follows_cwd_changes() {
    let d = tempfile_dir("cwd");
    let (mut a, _rx) = app(vec![]);
    // Read once for the old folder (it doesn't exist: an empty list).
    typ(&mut a, "@new");
    assert!(!screen(&mut a, 60, 12).contains("▸"));
    a.editor.clear();
    std::fs::write(d.join("newfile.txt"), "").unwrap();
    a.on_agent(AgentEvent::CwdChanged {
        cwd: d.display().to_string(),
        branch: None,
    });
    typ(&mut a, "@new");
    assert!(screen(&mut a, 60, 12).contains("▸ newfile.txt"));
}

#[test]
fn draft_event_keeps_history_and_pastes() {
    let (mut a, mut rx) = app(vec![Message::user("old")]);
    typ(&mut a, "see ");
    a.on_event(Event::Paste("line\n".repeat(10)));
    a.on_ui(UiEvent::Draft { text: "queued".into() });
    assert_eq!(a.editor.text(), "queued\nsee [paste #1 +10 lines]");
    // History unchanged: Up at the top of the text still shows the last real prompt.
    a.editor.set_text("");
    key(&mut a, KeyCode::Up, KeyModifiers::NONE);
    assert_eq!(a.editor.text(), "old");
    a.editor.set_text("queued\nsee [paste #1 +10 lines]");
    key(&mut a, KeyCode::Enter, KeyModifiers::NONE);
    assert_eq!(
        rx.try_recv().unwrap(),
        Cmd::Prompt(format!("queued\nsee {}", "line\n".repeat(10)))
    );
}

/// A fresh folder per test (tests run in parallel).
fn tempfile_dir(name: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("nc-tui-{}-{name}", std::process::id()));
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
        s.contains("opus · high")
            && !s.lines().last().unwrap().contains("claude/opus")
            && s.contains("the cache is kept"),
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
    assert!(s.contains("∴ thought · 6 words · ctrl+t show"), "{s}");
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

/// L11.12 budget: keypress to frame < 16 ms with extension UI active, on a long
/// transcript. Timing-sensitive, so `scripts/bench.sh` runs it (`--ignored`), not check.sh.
#[test]
#[ignore]
fn bench_frame() {
    let mut history = Vec::new();
    for i in 0..300 {
        history.push(Message::user(format!("question {i}: why does `foo` fail?")));
        history.push(assistant(
            vec![
                call(&format!("c{i}"), "bash", json!({"command": "cargo test"})),
                Block::text(format!(
                    "## Answer {i}\n\nThe **bug** is in `foo`:\n\n```rust\nfn foo() -> u32 {{ {i} }}\n```\n\n- one\n- two\n"
                )),
            ],
            StopReason::Stop,
        ));
    }
    let (mut a, _rx) = app(history);
    for k in 0..5 {
        let op = |v| serde_json::from_value(v).unwrap();
        let ext = format!("e{k}");
        a.on_agent(AgentEvent::Ui {
            ext: ext.clone(),
            op: op(json!({"op": "status", "key": ext, "text": "ready"})),
        });
        a.on_agent(AgentEvent::Ui {
            ext: ext.clone(),
            op: op(json!({"op": "widget", "key": ext, "lines": ["line a", [{"text": "b", "style": "accent"}]]})),
        });
    }
    let (p50, p99) = frame_times(&mut a);
    println!("BENCH frame_p50_ms {:.2}", p50.as_secs_f64() * 1e3);
    println!("BENCH frame_p99_ms {:.2}", p99.as_secs_f64() * 1e3);
    assert!(p99.as_millis() < 16, "frame p99 {p99:?} over the 16 ms budget");
}

/// Keypress to frame: draw once, then time 200 keypresses each followed by a draw.
/// Returns (p50, p99).
fn frame_times(a: &mut App) -> (std::time::Duration, std::time::Duration) {
    let mut t = Terminal::new(TestBackend::new(200, 60)).unwrap();
    t.draw(|f| a.draw(f)).unwrap();
    let mut times = Vec::new();
    for i in 0..200 {
        let start = std::time::Instant::now();
        key(a, KeyCode::Char(if i % 2 == 0 { 'x' } else { 'y' }), KeyModifiers::NONE);
        t.draw(|f| a.draw(f)).unwrap();
        times.push(start.elapsed());
    }
    times.sort();
    (times[times.len() / 2], times[times.len() * 99 / 100])
}

/// L11.12 report-only row: a day-long session, 2000 turns, every turn expanded
/// (Alt+O), each with a tool call and its output. No budget: `scripts/bench.sh` prints it.
#[test]
#[ignore]
fn bench_frame_long() {
    let mut history = Vec::new();
    for i in 0..2000 {
        history.push(Message::user(format!("question {i}: why does `foo` fail?")));
        history.push(assistant(
            vec![call(&format!("c{i}"), "bash", json!({"command": "cargo test"}))],
            StopReason::ToolUse,
        ));
        history.push(result(
            &format!("c{i}"),
            "bash",
            &format!("running 3 tests\ntest a ... ok\ntest b ... ok\ntest foo ... FAILED {i}\n"),
            true,
        ));
        history.push(assistant(
            vec![Block::text(format!(
                "## Answer {i}\n\nThe **bug** is in `foo`:\n\n```rust\nfn foo() -> u32 {{ {i} }}\n```\n\n- one\n- two\n"
            ))],
            StopReason::Stop,
        ));
    }
    let (mut a, _rx) = app(history);
    key(&mut a, KeyCode::Char('o'), KeyModifiers::ALT);
    let (p50, p99) = frame_times(&mut a);
    println!("BENCH frame_long_p50_ms {:.2}", p50.as_secs_f64() * 1e3);
    println!("BENCH frame_long_p99_ms {:.2}", p99.as_secs_f64() * 1e3);
}

#[test]
fn prompt_echoes_at_once_and_thinking_ticks() {
    let (mut a, mut rx) = app(vec![]);
    for c in "fix it".chars() {
        a.on_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
    }
    a.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert_eq!(rx.try_recv().unwrap(), Cmd::Prompt("fix it".into()));
    // Before the agent answers, the prompt is already in the chat, once.
    assert_eq!(screen(&mut a, 50, 12).matches("fix it").count(), 1);
    a.on_agent(AgentEvent::AgentStart);
    a.on_agent(AgentEvent::Message(Message::user("fix it")));
    assert_eq!(screen(&mut a, 50, 12).matches("fix it").count(), 1);
    a.on_agent(AgentEvent::Stream(StreamEvent::BlockStart {
        index: 0,
        kind: BlockKind::Thinking,
        name: None,
    }));
    a.on_agent(AgentEvent::Stream(StreamEvent::Delta {
        index: 0,
        text: "one two three".into(),
    }));
    while a.pace() {}
    assert!(screen(&mut a, 50, 12).contains("thinking"), "live thinking line");
    // A prompt that never comes back (refused by a hook) loses its echo when the run settles.
    a.on_agent(AgentEvent::Settled);
    for c in "again".chars() {
        a.on_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
    }
    a.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    a.on_agent(AgentEvent::Settled);
    assert!(!screen(&mut a, 50, 12).contains("again"));
}

/// TP9 (G3): a new session opens with version, folder, model and one hint, as the first item.
#[test]
fn startup_header_on_a_new_session() {
    let (mut a, mut rx) = app(vec![]);
    let s = screen(&mut a, 60, 12);
    let lines: Vec<&str> = s.lines().collect();
    assert_eq!(lines[0], format!(" novacode {}", env!("CARGO_PKG_VERSION")), "{s}");
    assert_eq!(lines[1], " /home/u/proj · claude/opus", "{s}");
    assert_eq!(lines[2], " type / for commands", "{s}");
    // Only drawn: nothing reaches the agent, so the session file never gets it.
    assert!(rx.try_recv().is_err());
    // It scrolls away like any message: a prompt follows it.
    for c in "hi".chars() {
        a.on_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
    }
    a.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert!(matches!(rx.try_recv(), Ok(Cmd::Prompt(p)) if p == "hi"));
    assert!(screen(&mut a, 60, 12).starts_with(" novacode "));
}

/// TP9: no header on resume, nor in an attach window (an existing session).
#[test]
fn no_startup_header_on_resume() {
    let header = format!("novacode {}", env!("CARGO_PKG_VERSION"));
    let (mut a, _rx) = app(vec![Message::User {
        content: vec![Block::text("earlier")],
    }]);
    let s = screen(&mut a, 60, 12);
    assert!(s.contains("earlier") && !s.contains(&header), "{s}");
    let (mut b, _rx) = app_at(vec![], Some("01a1"));
    let s = screen(&mut b, 60, 12);
    assert!(!s.contains(&header), "{s}");
}

#[test]
fn reload_during_a_turn_waits_for_it_to_end() {
    let (mut a, _rx) = app(vec![]);
    let type_ = |a: &mut App, s: &str| {
        for c in s.chars() {
            a.on_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        }
        a.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    };
    a.on_agent(AgentEvent::AgentStart);
    type_(&mut a, "/reload");
    assert_eq!(a.exit, None, "the turn keeps going");
    assert!(screen(&mut a, 70, 12).contains("reload queued"));
    a.on_agent(AgentEvent::Settled);
    assert_eq!(a.exit, Some(Exit::Reload));
    // Idle: at once.
    let (mut b, _rx) = app(vec![]);
    type_(&mut b, "/reload");
    assert_eq!(b.exit, Some(Exit::Reload));
}

/// One approval card for `rm -rf target`, past its grace; the receiver gets the answer.
fn approve_rm(a: &mut App) -> tokio::sync::oneshot::Receiver<Answer> {
    let (tx, reply) = tokio::sync::oneshot::channel();
    a.on_ui(UiEvent::Approve(ApproveReq {
        id: 1,
        tool: "bash".into(),
        detail: "rm -rf target".into(),
        reason: "deletes files".into(),
        rule: "Bash(rm -rf:*)".into(),
        risky: false,
        reply: tx,
    }));
    a.age_approvals();
    reply
}

/// TP10: on a narrow card the hint line drops from the end, so deny is never the one lost.
#[test]
fn approval_card_keeps_deny_on_a_narrow_screen() {
    let (mut a, _rx) = app(vec![]);
    let _reply = approve_rm(&mut a);
    let s = screen(&mut a, 34, 24);
    assert!(s.contains("y once · n deny"), "{s}");
}

/// TP10: a narrow ask form keeps `esc dismiss` right after the main key.
#[test]
fn ask_form_keeps_dismiss_on_a_narrow_screen() {
    let (mut a, _rx) = app(vec![]);
    let _answers = ask_db(&mut a, 1);
    let s = screen(&mut a, 36, 40);
    assert!(overlay_keys(&s).contains("esc dismiss"), "{s}");
}

/// The Files tab with `src/a.rs`, focus on the input; returns the file row's screen row.
fn files_tab(a: &mut App) -> u16 {
    a.on_agent(AgentEvent::ToolStart {
        call: ToolCall {
            id: "e".into(),
            name: "edit".into(),
            arguments: json!({"path": "src/a.rs"}),
            arguments_error: None,
            replay: None,
        },
    });
    a.panel.visible = true;
    a.panel.show_tab("files");
    row_of(&screen(a, 120, 24), "src/a.rs")
}

/// TP10: while an approval card or ask form is open, a click in the panel neither acts
/// on a row nor moves focus there.
#[test]
fn panel_clicks_do_nothing_while_a_dialog_is_open() {
    let (mut a, mut rx) = app(vec![]);
    let x = 120 - a.panel.width(120) + 4;
    let row = files_tab(&mut a);
    let _reply = approve_rm(&mut a);
    screen(&mut a, 120, 24);
    while rx.try_recv().is_ok() {}
    click(&mut a, x, row);
    click(&mut a, x, row);
    assert!(rx.try_recv().is_err(), "no file opens under an approval card");
    assert_eq!(a.focus(), Focus::Dialog);
    key(&mut a, KeyCode::Char('y'), KeyModifiers::NONE);
    assert_eq!(a.focus(), Focus::Input, "the panel did not take focus");

    let (mut a, mut rx) = app(vec![]);
    let row = files_tab(&mut a);
    let _answers = ask_db(&mut a, 1);
    screen(&mut a, 120, 24);
    while rx.try_recv().is_ok() {}
    click(&mut a, x, row);
    click(&mut a, x, row);
    assert!(rx.try_recv().is_err(), "no file opens under an ask form");
    key(&mut a, KeyCode::Esc, KeyModifiers::NONE);
    assert_eq!(a.focus(), Focus::Input, "the panel did not take focus");
    // With no dialog open, the same click opens the file.
    click(&mut a, x, row);
    assert_eq!(a.focus(), Focus::Panel);
    assert!(
        std::iter::from_fn(|| rx.try_recv().ok()).any(|c| matches!(c, Cmd::OpenFile(_))),
        "a click opens the file when nothing is pending"
    );
}

/// TP10: a focused panel passes App keys it does not use, such as the keys help.
#[test]
fn focused_panel_lets_app_keys_through() {
    let (mut a, _rx) = app(vec![]);
    key(&mut a, FOCUS_PANEL.0, FOCUS_PANEL.1);
    assert_eq!(a.focus(), Focus::Panel);
    key(&mut a, KeyCode::Char('/'), KeyModifiers::ALT);
    let s = screen(&mut a, 120, 30);
    assert_eq!(a.focus(), Focus::Dialog, "the keys help opened: {s}");
    // Keys the panel does not use and the app does not bind still don't reach the input.
    key(&mut a, KeyCode::Esc, KeyModifiers::NONE);
    key(&mut a, FOCUS_PANEL.0, FOCUS_PANEL.1);
    key(&mut a, KeyCode::Char('z'), KeyModifiers::NONE);
    assert!(a.editor.is_empty());
}

/// TP10: status icons differ in colour by state (docs/tui-style.md "Glyphs").
#[test]
fn task_icons_are_coloured_by_state() {
    let t = novacode_tui::theme::Theme::default();
    let (mut a, _rx) = app(vec![]);
    let row = |label: &str, state: &str| novacode_tui::panel::TaskRow {
        label: label.into(),
        state: state.into(),
        ..Default::default()
    };
    a.on_ui(UiEvent::Board {
        // The first row is selected, so its text is drawn dim; the others keep their colours.
        tasks: vec![
            row("T0-sel", "queued"),
            row("T1-run", "running"),
            row("T2-ok", "ready"),
            row("T3-bad", "failed"),
        ],
        needs: vec![],
    });
    a.panel.visible = true;
    a.panel.show_tab("tasks");
    let buf = draw(&mut a, 120, 24);
    let x0 = 120 - a.panel.width(120);
    for (label, glyph, colour) in [("T1-run", "◐", t.warn), ("T2-ok", "✓", t.ok), ("T3-bad", "✗", t.error)] {
        let y = row_of(&text(&buf), label);
        let x = (x0..120)
            .find(|&x| buf[(x, y)].symbol() == glyph)
            .unwrap_or_else(|| panic!("{label}: no {glyph}"));
        assert_eq!(buf[(x, y)].fg, colour.fg.unwrap(), "{label} icon colour");
    }
}
