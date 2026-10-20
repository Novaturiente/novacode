//! App state, input handling and drawing. Talks to the session only through
//! [`AgentEvent`]s and [`UiEvent`]s in and [`Cmd`]s out, so the same UI can
//! later sit on an attach socket (L8.13).

use crate::dialog::{self, Answer, ApprovalCard, ApproveReq, Pager, PickItem, Picked, Picker};
use crate::editor::{Action, Editor};
use crate::keys::{A, Keymap, Scope};
use crate::theme::Theme;
use crate::transcript::{Thinking, Transcript, View};
use crate::wrap;
use crossterm::event::{Event, KeyEvent, KeyEventKind, MouseButton, MouseEvent, MouseEventKind};
use novacode_types::{AgentEvent, BlockKind, Cost, Message, StopReason, StreamEvent, ThinkingLevel, Usage};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Padding, Paragraph};
use std::collections::VecDeque;
use std::time::{Duration, Instant};
use tokio::sync::mpsc::UnboundedSender;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

const EDITOR_MAX_ROWS: usize = 10;
const WHEEL: isize = 3;
const DOUBLE_PRESS: Duration = Duration::from_millis(500);
const COMPLETE_ROWS: usize = 8;
/// Files listed for `@` completion; bigger trees are cut.
const FILE_CAP: usize = 20_000;
const SPINNER: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// Built-in slash commands (L8.9). Skill and template commands come from the
/// host (`Init.commands`); over RPC the host runs the session ones itself.
pub const COMMANDS: &[(&str, &str)] = &[
    ("/model", "pick a model, or /model <id>"),
    ("/thinking", "pick a thinking level, or /thinking <level>"),
    ("/resume", "resume another session in this folder"),
    ("/new", "start a new session"),
    ("/keys", "show keybindings"),
    ("/context", "where the context window goes"),
    ("/compact", "replace older messages with the memory notes"),
    ("/tree", "go to any prompt of the session tree"),
    ("/rewind", "go back to before a prompt of this branch"),
    ("/fork", "new session up to a prompt of this branch"),
    ("/clone", "new session with this whole branch"),
    ("/export", "write this branch to a file (/export html for a web page)"),
    ("/copy", "copy the last reply"),
    ("/btw", "side question: /btw <question>, not kept in the conversation"),
    ("/name", "name this session: /name <text>"),
    (
        "/worktree",
        "git worktrees: list, /worktree <name> to enter or create, /worktree exit or remove to leave",
    ),
    (
        "/login",
        "log in: /login <provider> (claude, codex, antigravity, copilot, or an API-key provider)",
    ),
    ("/ps", "processes in the process manager, with their latest output"),
    ("/mcp", "MCP servers: status and tool counts"),
    ("/reload", "re-read settings, keys, theme, skills and context files"),
    ("/restart", "restart novacode (a new build) and resume this session"),
    ("/quit", "quit"),
];

/// What the UI knows at start.
pub struct Init {
    /// `provider/model`.
    pub model: String,
    pub cwd: String,
    pub branch: Option<String>,
    pub window_tokens: u64,
    /// Messages of a resumed session.
    pub history: Vec<Message>,
    pub keymap: Keymap,
    /// Fold finished turns (L9.8, setting `ui.turnCollapse`).
    pub collapse_turns: bool,
    /// Startup warnings (bad keybindings, settings).
    pub notices: Vec<String>,
    /// Startup notes that aren't problems (a rewind's result, a reload).
    pub info: Vec<String>,
    pub theme: Theme,
    /// Setting `ui.thinking`: collapsed, full or hidden.
    pub thinking: Thinking,
    /// Thinking level in use (L2.16).
    pub level: ThinkingLevel,
    /// API prices of the current route; `None` on subscriptions, where no cost is shown (L2.21).
    pub price: Option<Cost>,
    /// USD already spent on API-key routes in this session (resumed history).
    pub spent: f64,
    /// Extra slash commands for completion: prompt templates and `/skill:name` (L4.10, L4.11).
    pub commands: Vec<(String, String)>,
    /// Editor text to start with (a prompt taken back by `/rewind` or `/tree`).
    pub draft: String,
    /// An `attach` window (L8.13): commands that swap the session stay with its own window.
    pub attached: bool,
    /// `ui.statusline` (L9.12).
    pub statusline: crate::statusline::Config,
    /// Only the side panel, full screen: the tmux panel pane (L9.18).
    pub panel_only: bool,
    /// `ui.panel.auto`: show the panel while work is going, hide it after (default on).
    pub panel_auto: bool,
    /// `ui.icons`: Nerd Font glyphs in the panel (default) or plain Unicode.
    pub nerd_icons: bool,
    /// Lower frame rate, no animation (SSH or a loaded machine, L9.17).
    pub slow: bool,
    /// `ui.images`: auto (default), kitty, iterm2, sixel, halfblocks or off (L9.14).
    pub images: String,
}

/// What the UI asks of the session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cmd {
    Prompt(String),
    /// Delivered before the next model call of the running turn (L3.7).
    Steer(String),
    Abort,
    /// `provider/model`, from the next run on; the host answers with [`UiEvent::ModelSet`] or a notice.
    SetModel(String),
    /// From the next message on; the host answers with [`UiEvent::Thinking`].
    SetThinking(ThinkingLevel),
    ListModels,
    ListSessions,
    /// The host answers with [`UiEvent::Branch`].
    ListPrompts(BranchAction),
    /// The host answers with [`UiEvent::Page`].
    Context,
    Compact,
    /// Write the session to a file; `true` = HTML (L4.14).
    Export(bool),
    /// The terminal gained or lost focus (L11.9: notify only when unwatched).
    Focus(bool),
    /// Ctrl+Alt+B: running bash commands move to the process manager (L5.8).
    Background,
    /// `/ps` and the Processes tab: the host answers with [`UiEvent::Procs`] (L5.11).
    Processes,
    /// `/mcp`: server status page (L7.4).
    Mcp,
    /// `/name` (L4.4).
    Rename(String),
    /// `/worktree [name|exit|remove]` (L5.22): the host lists, enters or leaves.
    Worktree(String),
    /// `/btw <question>`: a side answer, shown as a page, never stored.
    Btw(String),
    /// Alt+Up: take back the newest queued message; `true` = a steer. The host
    /// answers with [`UiEvent::Dequeued`].
    Dequeue(bool),
    /// A key or panel action for an extension (L9 UI API).
    ExtKey {
        ext: String,
        tab: Option<String>,
        node: Option<String>,
        key: String,
        width: u16,
        height: u16,
    },
    Quit,
}

/// Things the host tells the UI besides agent events. Serialisable for
/// attached windows (L8.13), except approvals, which travel as their own message.
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum UiEvent {
    /// Answer to [`Cmd::Processes`]: every listed process with its output tail.
    Procs {
        items: Vec<crate::procs::ProcView>,
    },
    /// Text to show in the pager (e.g. `/context`).
    Page {
        title: String,
        text: String,
    },
    #[serde(skip)]
    Approve(ApproveReq),
    /// `ask_user` questions (L5.21).
    #[serde(skip)]
    Ask(crate::ask::AskReq),
    /// Another window answered questions `id`: drop the form.
    AskDone {
        id: u64,
    },
    /// Another window answered approval `id`: drop its card.
    ApprovalDone {
        id: u64,
    },
    Models {
        result: Result<Vec<PickItem>, String>,
    },
    Sessions {
        items: Vec<PickItem>,
    },
    /// Prompts to pick for `/tree`, `/rewind` or `/fork`; values are entry ids.
    Branch {
        action: BranchAction,
        items: Vec<PickItem>,
    },
    Notice {
        text: String,
        error: bool,
    },
    /// Answer to [`Cmd::Dequeue`]: `false` = it already reached the model.
    Dequeued {
        ok: bool,
    },
    /// The level actually used (clamped to what the model supports), and a note.
    Thinking {
        level: ThinkingLevel,
        note: String,
    },
    /// Host facts for the status line and Usage tab: quota windows (% used) and MCP servers.
    Info {
        five_hour: Option<f64>,
        weekly: Option<f64>,
        mcp_ready: usize,
        mcp_total: usize,
    },
    /// A model switch was accepted: the resolved `provider/model`, its context window and level.
    ModelSet {
        model: String,
        window_tokens: u64,
        level: ThinkingLevel,
        price: Option<Cost>,
    },
}

/// Why the UI stopped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Exit {
    Quit,
    /// Session id or path picked in /resume.
    Resume(String),
    New,
    /// Branch at a prompt (its entry id), or at the leaf for `/clone` (L4.3).
    Branch(BranchAction, Option<String>),
    /// `/rewind` to before a prompt: its files, its conversation or both (L4.15).
    /// `at = "undo"` undoes the last file rewind.
    Rewind {
        at: String,
        files: bool,
        chat: bool,
    },
    /// `/reload` (L8.10): same session, settings read again.
    Reload,
    /// `/restart` (L8.10): exec the binary again on this session.
    Restart,
}

/// What picking a prompt does (L4.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BranchAction {
    /// `/tree`: move to any prompt of any branch, its text back in the editor.
    Tree,
    /// `/rewind`: back to a prompt of this branch, its text back in the editor.
    Rewind,
    /// `/fork`: a new session up to a prompt of this branch.
    Fork,
    /// `/clone`: a new session with this whole branch.
    Clone,
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Pos {
    line: usize,
    col: usize,
}

struct Shown {
    view: View,
    top: usize,
    area: Rect,
}

/// Popup under the cursor: `@file` or a slash command (L9.5).
struct Completion {
    /// Byte where the completed token starts.
    start: usize,
    /// (label, text to insert, detail).
    items: Vec<(String, String, String)>,
    sel: usize,
}

/// Transcript search (L9.9).
struct Search {
    query: String,
    /// Absolute view line of the current match.
    at: Option<usize>,
}

enum Overlay {
    Models(Picker),
    Thinking(Picker),
    Sessions(Picker),
    Branch(BranchAction, Picker),
    /// After a prompt is picked in `/rewind`: what to restore.
    RewindMode(String, Picker),
    Pager(Pager),
}

pub struct App {
    pub theme: Theme,
    pub tr: Transcript,
    pub editor: Editor,
    pub keymap: Keymap,
    tx: UnboundedSender<Cmd>,
    init: Init,
    pub busy: bool,
    /// Running processes in the manager (L5.11), for the status line.
    procs: std::collections::BTreeSet<String>,
    activity: String,
    pub spinner: usize,
    usage: Option<Usage>,
    /// Context in use: the last reply's usage plus estimates since (L2.21).
    ctx: u64,
    /// Steering messages sent but not yet delivered.
    /// Queued while busy: (text, steer).
    pending: Vec<(String, bool)>,
    /// `None` = follow the tail; otherwise (turn, line offset in turn) at the top.
    anchor: Option<(usize, usize)>,
    shown: Option<Shown>,
    selection: Option<(Pos, Pos)>,
    flash: Option<(String, Instant)>,
    last_ctrl_c: Option<Instant>,
    last_esc: Option<Instant>,
    editor_width: usize,
    approvals: VecDeque<ApprovalCard>,
    asks: VecDeque<crate::ask::AskForm>,
    overlay: Option<Overlay>,
    completion: Option<Completion>,
    files: Option<Vec<String>>,
    search: Option<Search>,
    pub dirty: bool,
    pub exit: Option<Exit>,
    pub suspend: bool,
    /// Open `$EDITOR` on the prompt (needs the terminal, so the loop does it).
    pub external_editor: bool,
    /// `/login <provider>`: the run loop hands the terminal to `novacode login`.
    pub login: Option<String>,
    pub panel: crate::panel::Panel,
    /// Extension status segments `ext:<ext>:<key>` → (text, style).
    ext_status: std::collections::BTreeMap<String, (String, Option<String>)>,
    /// Extension widgets → (lines, below the editor).
    widgets: std::collections::BTreeMap<String, (Vec<novacode_types::ui::Line>, bool)>,
    footer: Option<Vec<novacode_types::ui::Line>>,
    header: Option<Vec<novacode_types::ui::Line>>,
    editor_title: Option<String>,
    editor_border: Option<String>,
    /// (key, extension) shortcuts.
    shortcuts: Vec<(String, String)>,
    quota: (Option<f64>, Option<f64>),
    mcp: (usize, usize),
    task_started: Option<Instant>,
    started: Instant,
    #[cfg(feature = "images")]
    images: Option<crate::images::Images>,
}

fn text_of(content: &[novacode_types::Block]) -> String {
    content
        .iter()
        .filter_map(|b| match b {
            novacode_types::Block::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

impl App {
    pub fn new(mut init: Init, tx: UnboundedSender<Cmd>) -> Self {
        let mut tr = Transcript {
            expand_all: !init.collapse_turns,
            thinking: init.thinking,
            thinking_hint: init.keymap.keys(A::ThinkingToggle),
            tools_hint: init.keymap.keys(A::ToolsExpand),
            ..Default::default()
        };
        let mut history = Vec::new();
        let mut usage = None;
        let ctx = novacode_types::estimate::context(&init.history);
        let mut panel = crate::panel::Panel::default();
        panel.nerd = init.nerd_icons;
        if init.panel_only {
            panel.pinned = Some(true);
            panel.focused = true;
        }
        if !init.panel_auto {
            panel.pinned.get_or_insert(false);
            panel.never_auto = true;
        }
        for m in std::mem::take(&mut init.history) {
            panel.on_agent(&AgentEvent::Message(m.clone()), init.window_tokens, init.price.as_ref());
            if let Message::User { content } = &m {
                history.push(text_of(content));
            }
            if let Message::Assistant(a) = &m
                && a.usage.prompt_total() > 0
            {
                usage = Some(a.usage);
            }
            tr.on_message(&m);
        }
        for n in std::mem::take(&mut init.notices) {
            tr.notice(n, true);
        }
        for n in std::mem::take(&mut init.info) {
            tr.notice(n, false);
        }
        tr.settle();
        Self {
            theme: std::mem::take(&mut init.theme),
            tr,
            editor: {
                let mut e = Editor::with_history(history);
                if !init.draft.is_empty() {
                    e.set_text(&init.draft);
                }
                e
            },
            keymap: std::mem::take(&mut init.keymap),
            tx,
            init,
            busy: false,
            procs: Default::default(),
            activity: String::new(),
            spinner: 0,
            usage,
            ctx,
            pending: Vec::new(),
            anchor: None,
            shown: None,
            selection: None,
            flash: None,
            last_ctrl_c: None,
            last_esc: None,
            editor_width: 80,
            approvals: VecDeque::new(),
            asks: VecDeque::new(),
            overlay: None,
            completion: None,
            files: None,
            search: None,
            dirty: true,
            exit: None,
            suspend: false,
            external_editor: false,
            login: None,
            panel,
            ext_status: Default::default(),
            widgets: Default::default(),
            footer: None,
            header: None,
            editor_title: None,
            editor_border: None,
            shortcuts: Vec::new(),
            quota: (None, None),
            mcp: (0, 0),
            task_started: None,
            started: Instant::now(),
            #[cfg(feature = "images")]
            images: None,
        }
    }

    fn send(&mut self, c: Cmd) {
        if self.tx.send(c).is_err() {
            self.exit = Some(Exit::Quit);
        }
    }

    fn flash(&mut self, s: impl Into<String>) {
        self.flash = Some((s.into(), Instant::now()));
    }

    pub fn on_ui(&mut self, ev: UiEvent) {
        self.dirty = true;
        match ev {
            UiEvent::Approve(req) => self.approvals.push_back(ApprovalCard::new(req)),
            UiEvent::ApprovalDone { id } => self.approvals.retain(|c| c.req.id != id),
            UiEvent::Ask(req) => self.asks.push_back(crate::ask::AskForm::new(req)),
            UiEvent::AskDone { id } => self.asks.retain(|f| f.req.id != id),
            UiEvent::Models { result: Ok(items) } => self.overlay = Some(Overlay::Models(Picker::new("Model", items))),
            UiEvent::Models { result: Err(e) } => self.tr.notice(format!("could not list models: {e}"), true),
            UiEvent::Sessions { items } if items.is_empty() => self.flash("no other sessions in this folder"),
            UiEvent::Sessions { items } => self.overlay = Some(Overlay::Sessions(Picker::new("Resume session", items))),
            UiEvent::Branch { items, .. } if items.is_empty() => self.flash("no prompts yet"),
            UiEvent::Branch { action: a, items } => {
                let title = match a {
                    BranchAction::Tree => "Session tree: go to a prompt (● = current branch)",
                    BranchAction::Rewind => "Rewind to before a prompt",
                    BranchAction::Fork | BranchAction::Clone => "Fork a new session before a prompt",
                };
                self.overlay = Some(Overlay::Branch(a, Picker::new(title, items)));
            }
            UiEvent::Notice { text, error } => self.tr.notice(text, error),
            UiEvent::Dequeued { ok: true } => {
                if let Some((t, _)) = self.pending.pop() {
                    // The draft goes after it, so nothing typed is lost.
                    let draft = self.editor.take().unwrap_or_default();
                    self.editor
                        .set_text(&if draft.is_empty() { t } else { format!("{t}\n{draft}") });
                }
            }
            UiEvent::Dequeued { ok: false } => self.flash("too late: it already reached the model"),
            UiEvent::Procs { items } => {
                if items.is_empty() && self.panel.procs_shown() {
                    self.panel.hide();
                    self.flash("no processes (long-running commands land here)");
                }
                self.panel.set_procs(items);
            }
            UiEvent::Page { title, text } => self.overlay = Some(Overlay::Pager(Pager::new(title, &text, &self.theme))),
            UiEvent::Thinking { level, note } => {
                self.init.level = level;
                self.tr.notice(note, false);
            }
            UiEvent::Info {
                five_hour,
                weekly,
                mcp_ready,
                mcp_total,
            } => {
                self.quota = (five_hour, weekly);
                self.mcp = (mcp_ready, mcp_total);
                self.panel.quota(five_hour, weekly);
            }
            UiEvent::ModelSet {
                model,
                window_tokens,
                level,
                price,
            } => {
                self.init.price = price;
                // L1.7: a model change re-sends the whole prompt uncached; say so, never block.
                let est = self.usage.map_or(0, |u| u.prompt_total());
                let mut note = format!("model → {model} · {} from the next message", level.as_str());
                if est > 0 && model != self.init.model {
                    note += &format!(" (the cache starts over: ~{}k tokens re-sent)", est / 1000);
                }
                self.init.model = model;
                self.init.window_tokens = window_tokens;
                self.init.level = level;
                self.tr.notice(note, false);
            }
        }
    }

    pub fn on_agent(&mut self, ev: AgentEvent) {
        self.dirty = true;
        self.panel
            .on_agent(&ev, self.init.window_tokens, self.init.price.as_ref());
        // Open at once on new todos (subagents: in `ui_op`), even while this session
        // is idle (the 1 s ticker only runs while busy or shown).
        if matches!(ev, AgentEvent::Message(Message::ToolResult { .. })) {
            self.panel.tick();
        }
        match &ev {
            AgentEvent::AgentStart => self.task_started = Some(Instant::now()),
            AgentEvent::Settled => self.task_started = None,
            _ => {}
        }
        match ev {
            AgentEvent::AgentStart => {
                self.busy = true;
                self.activity = "working".into();
            }
            AgentEvent::TurnStart => self.activity = "waiting for the model".into(),
            AgentEvent::Stream(e) => {
                if let StreamEvent::BlockStart { kind, name, .. } = &e {
                    self.activity = match kind {
                        BlockKind::Thinking => "thinking".into(),
                        BlockKind::ToolCall => format!("writing {} call", name.as_deref().unwrap_or("tool")),
                        _ => "writing".into(),
                    };
                }
                if let StreamEvent::Notice { text } = &e {
                    self.tr.notice(text.clone(), false);
                }
                self.tr.on_stream(&e);
            }
            AgentEvent::Message(m) => {
                if let Message::User { content } = &m {
                    let text = text_of(content);
                    if let Some(i) = self.pending.iter().position(|(p, _)| *p == text) {
                        self.pending.remove(i);
                    }
                }
                match &m {
                    Message::Assistant(a) if a.usage.prompt_total() > 0 => {
                        self.ctx = a.usage.prompt_total() + a.usage.output;
                        if let Some(p) = &self.init.price {
                            self.init.spent += a.usage.cost(p);
                        }
                    }
                    other => self.ctx += novacode_types::estimate::message(other),
                }
                if let Message::Assistant(a) = &m {
                    if a.usage.prompt_total() > 0 {
                        self.usage = Some(a.usage);
                    }
                    if a.stop == StopReason::Aborted {
                        self.tr.notice("interrupted", false);
                    }
                }
                self.tr.on_message(&m);
            }
            AgentEvent::ToolStart { call } => self.activity = format!("running {}", call.name),
            AgentEvent::ToolUpdate { call_id, text } => self.tr.tool_update(&call_id, &text),
            AgentEvent::ToolEnd {
                call_id,
                is_error,
                duration_ms,
            } => self.tr.tool_end(&call_id, is_error, duration_ms),
            AgentEvent::Retry {
                attempt,
                delay_ms,
                message,
                ..
            } => {
                self.activity = "retrying".into();
                self.tr.notice(
                    format!("retry {attempt} in {:.0}s: {message}", delay_ms as f64 / 1000.0),
                    false,
                );
            }
            AgentEvent::AgentEnd { .. } => {}
            AgentEvent::CwdChanged { cwd, branch } => {
                self.init.cwd = cwd;
                self.init.branch = branch;
            }
            AgentEvent::ProcessStarted { id, name } => {
                self.tr
                    .notice(format!("⚙ {id} {name}: running in the process manager (/ps)"), false);
                self.procs.insert(id);
            }
            AgentEvent::ProcessEnded {
                id,
                name,
                code,
                stopped,
            } => {
                let how = match (stopped, code) {
                    (true, _) => "stopped".to_string(),
                    (_, Some(0)) => "exited".into(),
                    (_, Some(c)) => format!("exited with code {c}"),
                    (_, None) => "killed".into(),
                };
                self.tr
                    .notice(format!("⚙ {id} {name}: {how}"), !stopped && code != Some(0));
                self.procs.remove(&id);
            }
            AgentEvent::Compacted {
                tokens_before,
                tokens_after,
                notes,
                fallback,
            } => {
                self.ctx = tokens_after;
                let how = if fallback {
                    "no notes were available, earlier messages were dropped".to_string()
                } else {
                    format!("{notes} notes")
                };
                self.tr.notice(
                    format!(
                        "compacted: {}k → {}k tokens ({how})",
                        tokens_before / 1000,
                        tokens_after / 1000
                    ),
                    false,
                );
            }
            AgentEvent::Ui { ext, op } => self.ui_op(&ext, op),
            AgentEvent::Settled => {
                self.busy = false;
                self.activity.clear();
                self.tr.settle();
            }
            AgentEvent::PromptTakenBack { text } => {
                self.tr.take_back();
                // Anything typed meanwhile stays, after the restored prompt.
                let typed = self.editor.text().to_string();
                let all = if typed.trim().is_empty() {
                    text
                } else {
                    format!("{text}\n{typed}")
                };
                self.editor.set_text(&all);
            }
        }
    }

    /// An extension's UI op (L9 UI API).
    fn ui_op(&mut self, ext: &str, op: novacode_types::ui::UiOp) {
        use novacode_types::ui::UiOp;
        match &op {
            UiOp::Status { key, text, style } => {
                let k = format!("ext:{key}");
                match text {
                    Some(t) => {
                        self.ext_status.insert(k, (t.clone(), style.clone()));
                    }
                    None => {
                        self.ext_status.remove(&k);
                    }
                }
            }
            UiOp::Widget { key, lines, below } => {
                let k = format!("{ext}:{key}");
                match lines {
                    Some(l) => {
                        self.widgets.insert(k, (l.clone(), *below));
                    }
                    None => {
                        self.widgets.remove(&k);
                    }
                }
            }
            UiOp::Footer { lines } => self.footer = lines.clone(),
            UiOp::Header { lines } => self.header = lines.clone(),
            UiOp::Editor { title, border } => {
                self.editor_title = title.clone();
                self.editor_border = border.clone();
            }
            UiOp::Shortcut { key, .. } => {
                self.shortcuts.retain(|(k, _)| k != key);
                self.shortcuts.push((key.clone(), ext.to_string()));
            }
            _ => {
                self.panel.apply(ext, &op);
                self.panel.tick();
            }
        }
    }

    fn panel_action(&mut self, a: crate::panel::PanelAction) {
        use crate::panel::PanelAction;
        match a {
            PanelAction::Command(c) => self.submit(c, self.busy),
            PanelAction::Ext { ext, tab, node, action } => {
                let (width, height) = self.panel.size;
                self.send(Cmd::ExtKey {
                    ext,
                    tab: Some(tab),
                    node,
                    key: action,
                    width,
                    height,
                });
            }
        }
    }

    /// Turn on images (L9.14) with the detected terminal protocol.
    #[cfg(feature = "images")]
    pub fn set_images(&mut self, i: crate::images::Images) {
        self.images = Some(i);
        self.tr.image_rows = 16;
        for t in &mut self.tr.turns {
            t.touch();
        }
    }

    pub fn image_setting(&self) -> &str {
        &self.init.images
    }

    pub fn slow(&self) -> bool {
        self.init.slow
    }

    /// Advance timers: the panel's auto show/hide. Returns true when something changed.
    pub fn tick(&mut self) -> bool {
        let before = self.panel.visible;
        self.panel.tick();
        // Live output while the Processes tab is on screen (the ticker runs only then or while busy).
        if self.panel.procs_shown() && !self.init.attached {
            self.send(Cmd::Processes);
        }
        before != self.panel.visible || (self.panel.visible && self.busy)
    }

    pub fn on_event(&mut self, ev: Event) {
        self.dirty = true;
        match ev {
            Event::Key(k) if k.kind != KeyEventKind::Release => self.on_key(k),
            Event::Paste(s) if self.approvals.is_empty() && self.asks.is_empty() && self.overlay.is_none() => {
                self.editor.paste(&s)
            }
            Event::Mouse(m) if self.overlay.is_none() => self.on_mouse(m),
            Event::FocusGained => self.send(Cmd::Focus(true)),
            Event::FocusLost => self.send(Cmd::Focus(false)),
            _ => {}
        }
    }

    fn answer(&mut self, ans: Answer) {
        let Some(card) = self.approvals.pop_front() else { return };
        let r = &card.req;
        let what = format!("{} {}", r.tool, r.detail.lines().next().unwrap_or(""));
        // Kept in the transcript so the decision stays visible, even in collapsed turns (C5).
        let (text, error) = match &ans {
            Answer::Once => (format!("allowed once: {what}"), false),
            Answer::Session => (format!("allowed for this session: {what}"), false),
            Answer::Always => (format!("always allowed: {}", r.rule), false),
            Answer::Deny(Some(why)) => (format!("denied: {what} — {why}"), true),
            Answer::Deny(None) => (format!("denied: {what}"), true),
        };
        self.tr.notice(text, error);
        let _ = card.req.reply.send(ans);
    }

    pub fn on_key(&mut self, k: KeyEvent) {
        let now = Instant::now();
        self.selection = None;
        if let Some(card) = self.approvals.front_mut() {
            if let Some(ans) = card.handle(k) {
                self.answer(ans);
            }
            return;
        }
        if let Some(form) = self.asks.front_mut() {
            use crate::ask::Done;
            let done = form.handle(k);
            self.dirty = true;
            let replies = match done {
                Done::Pending => return,
                Done::Answered(r) => Some(r),
                Done::Dismissed => None,
            };
            let form = self.asks.pop_front().unwrap();
            // Kept in the transcript so the answer stays visible (C5).
            match &replies {
                Some(r) => {
                    for r in r {
                        let mut a = r.chosen.join(", ");
                        if let Some(t) = &r.text {
                            a = if a.is_empty() { t.clone() } else { format!("{a}; {t}") };
                        }
                        self.tr.notice(format!("{} → {a}", r.question), false);
                    }
                }
                None => self.tr.notice("questions dismissed", true),
            }
            let _ = form.req.reply.send(replies);
            return;
        }
        if let Some(o) = &mut self.overlay {
            let page = self.shown.as_ref().map_or(10, |s| s.area.height as usize);
            match o {
                Overlay::Pager(p) => {
                    if p.handle(k, page) {
                        self.overlay = None;
                    }
                }
                Overlay::Models(p)
                | Overlay::Sessions(p)
                | Overlay::Thinking(p)
                | Overlay::Branch(_, p)
                | Overlay::RewindMode(_, p) => match p.handle(k, &self.keymap) {
                    Picked::Pending => {}
                    Picked::Cancel => self.overlay = None,
                    Picked::Value(v) => {
                        let o = self.overlay.take();
                        match o {
                            Some(Overlay::Models(_)) => self.set_model(&v),
                            Some(Overlay::Thinking(_)) => self.set_thinking(&v),
                            Some(Overlay::Branch(BranchAction::Rewind, _)) if v == "undo" => {
                                self.exit = Some(Exit::Rewind {
                                    at: v,
                                    files: true,
                                    chat: false,
                                })
                            }
                            Some(Overlay::Branch(BranchAction::Rewind, _)) => {
                                let item = |label: &str, value: &str| PickItem {
                                    label: label.into(),
                                    detail: String::new(),
                                    value: value.into(),
                                };
                                let items = vec![
                                    item("Files and conversation", "both"),
                                    item("Conversation only", "chat"),
                                    item("Files only", "files"),
                                ];
                                self.overlay = Some(Overlay::RewindMode(v, Picker::new("Restore what?", items)));
                            }
                            Some(Overlay::Branch(a, _)) => self.exit = Some(Exit::Branch(a, Some(v))),
                            Some(Overlay::RewindMode(at, _)) => {
                                self.exit = Some(Exit::Rewind {
                                    at,
                                    files: v != "chat",
                                    chat: v != "files",
                                })
                            }
                            _ => self.exit = Some(Exit::Resume(v)),
                        }
                    }
                },
            }
            return;
        }
        if self.search.is_some() {
            self.search_key(k);
            return;
        }
        // The focused panel takes the keys, except the ones that leave it (L9.16).
        if self.panel.focused && self.panel.visible {
            match self.keymap.lookup(&k, &[Scope::App]) {
                Some(A::PanelFocus | A::PanelToggle | A::Clear) if !self.init.panel_only => {}
                Some(A::Clear) => self.exit = Some(Exit::Quit),
                _ => {
                    if let Some(Some(a)) = self.panel.key(&crate::keys::name(&k)) {
                        self.panel_action(a);
                    }
                    if !self.panel.focused && self.init.panel_only {
                        self.panel.focused = true; // the panel pane has nothing else to focus
                    }
                    return;
                }
            }
        }
        if self.completion.is_some() && self.completion_key(k) {
            return;
        }
        let scopes: &[Scope] = if self.editor.searching() {
            &[Scope::Editor]
        } else {
            &[Scope::App, Scope::Scroll, Scope::Editor]
        };
        let mut a = self.keymap.lookup(&k, scopes);
        // Ctrl+D quits only on an empty editor; otherwise it deletes forward.
        if a == Some(A::Exit) && !self.editor.is_empty() {
            a = self.keymap.lookup(&k, &[Scope::Editor]);
        }
        match a {
            Some(A::Clear) => {
                if !self.editor.is_empty() {
                    self.editor.clear();
                } else if self.busy {
                    self.send(Cmd::Abort);
                } else if self.last_ctrl_c.is_some_and(|t| now - t < Duration::from_secs(1)) {
                    self.exit = Some(Exit::Quit);
                } else {
                    self.flash(format!("press {} again to quit", self.keymap.keys(A::Clear)));
                }
                self.last_ctrl_c = Some(now);
            }
            Some(A::Exit) => self.exit = Some(Exit::Quit),
            Some(A::Suspend) => self.suspend = true,
            Some(A::TurnsToggle) => self.tr.toggle_all(),
            Some(A::Interrupt) => {
                if self.busy {
                    self.send(Cmd::Abort);
                } else if self.last_esc.is_some_and(|t| now - t < DOUBLE_PRESS) {
                    self.editor.clear();
                }
                self.last_esc = Some(now);
            }
            Some(A::ModelSelect) => self.list_models(),
            Some(A::SessionResume) => self.send(Cmd::ListSessions),
            Some(A::SessionNew) => self.exit = Some(Exit::New),
            // pi's Ctrl+O: every tool's output in full, inline; again to fold it back.
            Some(A::ToolsExpand) => {
                let on = !self.tr.tools_full;
                self.tr.set_tools_full(on);
                self.flash(if on { "tool output: full" } else { "tool output: short" });
            }
            Some(A::KeysHelp) => self.overlay = Some(Overlay::Pager(Pager::keys_help(&self.keymap, &self.theme))),
            Some(A::MessageCopy) => self.copy_reply(),
            Some(A::InputCopy) => {
                let t = self.editor.full_text();
                if t.is_empty() {
                    self.flash("nothing to copy");
                } else {
                    crate::term::copy(&t);
                    self.flash(format!("copied the input ({} chars)", t.chars().count()));
                }
            }
            Some(A::Background) => self.send(Cmd::Background),
            Some(A::PasteImage) => match crate::term::paste() {
                Some(crate::term::Pasted::Image(path)) => {
                    self.editor.insert(&format!("@{} ", path.display()));
                }
                Some(crate::term::Pasted::Text(t)) => self.editor.paste(&t),
                None => self
                    .tr
                    .notice("nothing to paste (no wl-paste/xclip, or an empty clipboard)", true),
            },
            Some(A::ExternalEditor) => self.external_editor = true,
            Some(A::Dequeue) if self.init.attached => self.flash("Alt+Up works in the session's own window"),
            Some(A::Dequeue) => match self.pending.last() {
                Some((_, steer)) => self.send(Cmd::Dequeue(*steer)),
                None => self.flash("nothing queued"),
            },
            Some(A::Reload) if self.init.attached => self.flash("reload works in the session's own window"),
            Some(A::Reload) => self.exit = Some(Exit::Reload),
            Some(A::HistorySearch) => self.editor.search_history(),
            Some(A::FollowUp) => {
                if let Some(t) = self.editor.take() {
                    self.submit(t, false);
                }
            }
            Some(A::PageUp) => self.scroll_by(-(self.page() as isize)),
            Some(A::PageDown) => self.scroll_by(self.page() as isize),
            Some(A::Top) => self.scroll_by(isize::MIN / 2),
            Some(A::Bottom) => self.anchor = None,
            Some(A::PrevPrompt) => self.jump_prompt(-1),
            Some(A::NextPrompt) => self.jump_prompt(1),
            Some(A::Search) => {
                self.search = Some(Search {
                    query: String::new(),
                    at: None,
                })
            }
            Some(A::ThinkingCycle) => {
                let all = ThinkingLevel::ALL;
                let i = all.iter().position(|l| *l == self.init.level).unwrap_or(0);
                self.send(Cmd::SetThinking(all[(i + 1) % all.len()]));
            }
            Some(A::ThinkingToggle) => self.toggle_thinking(),
            Some(A::PanelToggle) => self.panel.toggle(),
            Some(A::PanelFocus) => {
                if !self.panel.visible {
                    self.panel.toggle();
                }
                self.panel.focused = !self.panel.focused;
            }
            None if let Some((key, ext)) = self.shortcuts.iter().find(|(s, _)| crate::keys::is(&k, s)).cloned() => {
                self.send(Cmd::ExtKey {
                    ext,
                    tab: None,
                    node: None,
                    key,
                    width: 0,
                    height: 0,
                });
            }
            a => {
                if let Action::Submit(text) = self.editor.handle(a, k, self.editor_width) {
                    self.completion = None;
                    self.submit(text, true);
                } else {
                    self.update_completion();
                }
            }
        }
    }

    /// Keys while the completion popup is open; `false` = not used, handle normally.
    fn completion_key(&mut self, k: KeyEvent) -> bool {
        use crossterm::event::KeyCode;
        let Some(c) = &mut self.completion else { return false };
        let n = c.items.len();
        match k.code {
            KeyCode::Up => c.sel = (c.sel + n - 1) % n,
            KeyCode::Down => c.sel = (c.sel + 1) % n,
            KeyCode::Esc => self.completion = None,
            _ => {
                let (start, insert) = (c.start, c.items[c.sel].1.clone());
                let token = &self.editor.text()[start..self.editor.cursor()];
                match self.keymap.lookup(&k, &[Scope::Editor]) {
                    Some(A::Tab) => {}
                    // Enter completes, unless the token is already complete (then it sends).
                    Some(A::Submit) if insert.trim_end() != token => {}
                    _ => return false,
                }
                self.editor.complete(start, &insert);
                self.completion = None;
                self.update_completion();
            }
        }
        true
    }

    fn update_completion(&mut self) {
        let text = self.editor.text();
        let before = &text[..self.editor.cursor()];
        let start = before.rfind(char::is_whitespace).map_or(0, |i| i + 1);
        let token = &before[start..];
        let items: Vec<(String, String, String)> = if start == 0 && token.starts_with('/') {
            COMMANDS
                .iter()
                .map(|(c, d)| (c.to_string(), d.to_string()))
                .chain(self.init.commands.iter().cloned())
                .filter(|(c, _)| c.starts_with(token))
                .map(|(c, d)| (c.clone(), format!("{c} "), d))
                .collect()
        } else if let Some(items) = token.strip_prefix('@').and_then(|q| browse(&self.init.cwd, q)) {
            items
        } else if let Some(q) = token.strip_prefix('@') {
            let files = self.files.get_or_insert_with(|| list_files(&self.init.cwd));
            let mut hits: Vec<(usize, &String)> = files
                .iter()
                .filter_map(|f| {
                    let base = f.trim_end_matches('/').rsplit('/').next().unwrap_or(f);
                    // A hit in the file name ranks above a hit spread over the path.
                    dialog::fuzzy(base, q)
                        .or_else(|| dialog::fuzzy(f, q).map(|s| s + 1000))
                        .map(|s| (s * 1000 + f.len(), f))
                })
                .collect();
            hits.sort();
            hits.into_iter()
                .take(50)
                // A folder keeps the popup open so you can go on into it.
                .map(|(_, f)| {
                    (
                        f.clone(),
                        format!("@{f}{}", if f.ends_with('/') { "" } else { " " }),
                        String::new(),
                    )
                })
                .collect()
        } else {
            Vec::new()
        };
        let done = items.len() == 1 && items[0].1.trim_end() == token;
        self.completion = (!items.is_empty() && !done).then_some(Completion { start, items, sel: 0 });
    }

    fn search_key(&mut self, k: KeyEvent) {
        use crossterm::event::{KeyCode, KeyModifiers};
        let Some(s) = &mut self.search else { return };
        let older = match k.code {
            KeyCode::Esc => {
                self.search = None;
                return;
            }
            KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                self.search = None;
                return;
            }
            KeyCode::Enter if k.modifiers.contains(KeyModifiers::SHIFT) => false,
            KeyCode::Enter | KeyCode::Up => true,
            KeyCode::Down => false,
            KeyCode::Char('g') if k.modifiers.contains(KeyModifiers::CONTROL) => true,
            KeyCode::Backspace => {
                s.query.pop();
                s.at = None;
                true
            }
            KeyCode::Char(c) if !k.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) => {
                s.query.push(c);
                s.at = None;
                true
            }
            _ => return,
        };
        self.search_step(older);
    }

    fn hits(&self) -> Vec<usize> {
        let (Some(s), Some(sh)) = (&self.search, &self.shown) else {
            return Vec::new();
        };
        if s.query.is_empty() {
            return Vec::new();
        }
        let q = s.query.to_lowercase();
        sh.view
            .lines
            .iter()
            .enumerate()
            .filter(|(_, l)| wrap::plain(l).to_lowercase().contains(&q))
            .map(|(i, _)| i)
            .collect()
    }

    /// Move to the next older (up) or newer match and scroll it into the middle.
    fn search_step(&mut self, older: bool) {
        let hits = self.hits();
        let Some(s) = &mut self.search else { return };
        let next = match s.at {
            None => hits.last().copied(),
            Some(at) if older => hits.iter().rev().find(|&&h| h < at).copied().or(Some(at)),
            Some(at) => hits.iter().find(|&&h| h > at).copied().or(Some(at)),
        };
        s.at = next.filter(|_| !hits.is_empty());
        if let (Some(line), Some(sh)) = (s.at, &self.shown) {
            let top = line.saturating_sub(sh.area.height as usize / 2);
            let delta = top as isize - sh.top as isize;
            self.scroll_by(delta);
        }
    }

    /// Stream pacer step; `true` = more text is waiting, draw again next frame.
    pub fn pace(&mut self) -> bool {
        self.tr.pace()
    }

    fn list_models(&mut self) {
        self.flash("loading models…");
        self.send(Cmd::ListModels);
    }

    fn set_model(&mut self, model: &str) {
        // A bare id means the current provider.
        let spec = if model.contains('/') {
            model.to_string()
        } else {
            let provider = self.init.model.split_once('/').map_or("", |(p, _)| p);
            format!("{provider}/{model}")
        };
        self.send(Cmd::SetModel(spec));
    }

    fn toggle_thinking(&mut self) {
        let next = if self.tr.thinking == Thinking::Full {
            Thinking::Collapsed
        } else {
            Thinking::Full
        };
        self.tr.set_thinking(next);
    }

    fn pick_thinking(&mut self) {
        let items = ThinkingLevel::ALL
            .iter()
            .map(|l| PickItem {
                label: l.as_str().into(),
                value: l.as_str().into(),
                detail: if *l == self.init.level {
                    "current".into()
                } else {
                    String::new()
                },
            })
            .collect();
        self.overlay = Some(Overlay::Thinking(
            Picker::new("Thinking level", items).select_value(self.init.level.as_str()),
        ));
    }

    fn set_thinking(&mut self, s: &str) {
        match ThinkingLevel::parse(s) {
            Some(l) => self.send(Cmd::SetThinking(l)),
            None => self.flash(format!(
                "unknown level `{s}`: off, minimal, low, medium, high, xhigh, max"
            )),
        }
    }

    fn copy_reply(&mut self) {
        match self.tr.last_reply() {
            Some(t) => {
                crate::term::copy(&t);
                self.flash(format!("copied the last reply ({} chars)", t.chars().count()));
            }
            None => self.flash("no reply to copy"),
        }
    }

    fn submit(&mut self, text: String, steer: bool) {
        let mut words = text.trim().splitn(2, char::is_whitespace);
        let (cmd, arg) = (words.next().unwrap_or(""), words.next().map(str::trim).unwrap_or(""));
        if COMMANDS.iter().any(|(c, _)| *c == cmd) || cmd == "/exit" || cmd == "/help" {
            if self.init.attached
                && matches!(
                    cmd,
                    "/resume" | "/new" | "/tree" | "/rewind" | "/fork" | "/clone" | "/reload" | "/restart" | "/login"
                )
            {
                self.tr.notice(
                    format!("{cmd} works in the session's own window, not an attached one"),
                    true,
                );
                self.dirty = true;
                return;
            }
            match cmd {
                "/quit" | "/exit" => self.exit = Some(Exit::Quit),
                "/model" if !arg.is_empty() => self.set_model(arg),
                "/model" => self.list_models(),
                "/thinking" if !arg.is_empty() => self.set_thinking(arg),
                "/thinking" => self.pick_thinking(),
                "/resume" => self.send(Cmd::ListSessions),
                "/new" => self.exit = Some(Exit::New),
                "/reload" => self.exit = Some(Exit::Reload),
                "/ps" => {
                    self.panel.show_procs();
                    self.send(Cmd::Processes);
                }
                "/mcp" => self.send(Cmd::Mcp),
                "/restart" => self.exit = Some(Exit::Restart),
                "/keys" | "/help" => {
                    self.overlay = Some(Overlay::Pager(Pager::keys_help(&self.keymap, &self.theme)));
                }
                "/copy" => self.copy_reply(),
                "/btw" if !arg.is_empty() => self.send(Cmd::Btw(arg.to_string())),
                "/btw" => self.flash("usage: /btw <question>"),
                "/name" if !arg.is_empty() => self.send(Cmd::Rename(arg.to_string())),
                "/name" => self.flash("usage: /name <text>"),
                "/worktree" => self.send(Cmd::Worktree(arg.to_string())),
                "/login" if !arg.is_empty() => self.login = Some(arg.to_string()),
                "/login" => self.flash("usage: /login <provider>, e.g. /login claude"),
                "/context" => self.send(Cmd::Context),
                "/compact" => self.send(Cmd::Compact),
                "/tree" => self.send(Cmd::ListPrompts(BranchAction::Tree)),
                "/rewind" => self.send(Cmd::ListPrompts(BranchAction::Rewind)),
                "/fork" => self.send(Cmd::ListPrompts(BranchAction::Fork)),
                "/clone" => self.exit = Some(Exit::Branch(BranchAction::Clone, None)),
                "/export" => self.send(Cmd::Export(arg.trim() == "html")),
                _ => {}
            }
            return;
        }
        self.anchor = None;
        // `!cmd` runs in the host now, even mid-turn; it never becomes a user message (L5.15).
        if text.starts_with('!') {
            self.send(Cmd::Prompt(text));
            return;
        }
        if self.busy {
            self.pending.push((text.clone(), steer));
            self.send(if steer { Cmd::Steer(text) } else { Cmd::Prompt(text) });
        } else {
            self.send(Cmd::Prompt(text));
        }
    }

    fn page(&self) -> usize {
        self.shown
            .as_ref()
            .map_or(10, |s| s.area.height.saturating_sub(2) as usize)
            .max(1)
    }

    fn top_turn(&self) -> Option<(usize, &Shown)> {
        let s = self.shown.as_ref()?;
        Some((s.view.starts.partition_point(|&st| st <= s.top).saturating_sub(1), s))
    }

    fn jump_prompt(&mut self, dir: isize) {
        let Some((turn, s)) = self.top_turn() else { return };
        let at_start = s.view.starts.get(turn) == Some(&s.top);
        let target = match dir {
            d if d < 0 && at_start => turn.checked_sub(1),
            d if d < 0 => Some(turn),
            _ => (turn + 1 < s.view.starts.len()).then_some(turn + 1),
        };
        match target {
            Some(t) => self.anchor = Some((t, 0)),
            None if dir > 0 => self.anchor = None,
            None => {}
        }
    }

    fn scroll_by(&mut self, delta: isize) {
        let Some(s) = &self.shown else { return };
        let max_top = s.view.lines.len().saturating_sub(s.area.height as usize);
        let top = (s.top as isize + delta).clamp(0, max_top as isize) as usize;
        if top >= max_top {
            self.anchor = None;
            return;
        }
        let turn = s.view.starts.partition_point(|&st| st <= top).saturating_sub(1);
        let start = s.view.starts.get(turn).copied().unwrap_or(0);
        self.anchor = Some((turn, top - start));
    }

    fn at(&self, col: u16, row: u16) -> Option<Pos> {
        let s = self.shown.as_ref()?;
        s.area.contains(Position::new(col, row)).then(|| Pos {
            line: s.top + (row - s.area.y) as usize,
            col: (col - s.area.x) as usize,
        })
    }

    fn on_mouse(&mut self, m: MouseEvent) {
        match m.kind {
            MouseEventKind::ScrollUp => self.scroll_by(-WHEEL),
            MouseEventKind::ScrollDown => self.scroll_by(WHEEL),
            MouseEventKind::Down(MouseButton::Left) => {
                self.selection = self.at(m.column, m.row).map(|p| (p, p));
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                if let (Some((a, _)), Some(p)) = (self.selection, self.at(m.column, m.row)) {
                    self.selection = Some((a, p));
                }
            }
            MouseEventKind::Up(MouseButton::Left) => {
                let Some((a, b)) = self.selection.take() else { return };
                if a == b {
                    let toggle = self
                        .shown
                        .as_ref()
                        .and_then(|s| s.view.toggles.iter().find(|(l, _)| *l == a.line).map(|(_, t)| *t));
                    let thought = self
                        .shown
                        .as_ref()
                        .and_then(|s| s.view.lines.get(a.line))
                        .is_some_and(|l| {
                            let p = wrap::plain(l);
                            p.starts_with("∴ ")
                        });
                    let image = self
                        .shown
                        .as_ref()
                        .and_then(|s| s.view.image_toggles.iter().find(|(l, _, _)| *l == a.line))
                        .map(|(_, t, i)| (*t, *i));
                    if let Some((t, i)) = image {
                        self.tr.toggle_image(t, i);
                    } else if let Some(t) = toggle {
                        self.tr.toggle(t);
                    } else if thought {
                        // A click on a thinking line shows or folds thinking, like Ctrl+T.
                        self.toggle_thinking();
                    }
                } else {
                    let text = self.selected_text(a.min(b), a.max(b));
                    if !text.is_empty() {
                        crate::term::copy(&text);
                        self.flash(format!("copied {} chars", text.chars().count()));
                    }
                }
            }
            _ => {}
        }
    }

    fn selected_text(&self, a: Pos, b: Pos) -> String {
        let Some(s) = &self.shown else { return String::new() };
        let mut out = Vec::new();
        for n in a.line..=b.line.min(s.view.lines.len().saturating_sub(1)) {
            let text = wrap::plain(&s.view.lines[n]);
            let from = if n == a.line { a.col } else { 0 };
            let to = if n == b.line { b.col + 1 } else { usize::MAX };
            let mut w = 0;
            let mut line = String::new();
            for g in text.graphemes(true) {
                if w >= from && w < to {
                    line.push_str(g);
                }
                w += g.width();
            }
            out.push(line.trim_end().to_string());
        }
        out.join("\n")
    }

    pub fn draw(&mut self, f: &mut Frame) {
        let full = f.area();
        if self.init.panel_only {
            self.panel.visible = true;
            self.panel.render(f, full, &self.theme);
            return;
        }
        // The side panel takes the right edge when the screen is wide enough (L9.16).
        let area = if self.panel.visible && full.width >= crate::panel::MIN_SCREEN {
            let w = self.panel.width(full.width);
            let [l, r] = Layout::horizontal([Constraint::Min(1), Constraint::Length(w)]).areas(full);
            self.panel.render(f, r, &self.theme);
            l
        } else {
            full
        };
        // Border plus one column of padding each side.
        let inner_w = area.width.saturating_sub(4) as usize;
        self.editor_width = inner_w;
        let ed_rows = self.editor.height(inner_w).clamp(1, EDITOR_MAX_ROWS);
        let mut notes: Vec<Line> = self
            .pending
            .iter()
            .rev()
            .take(3)
            .rev()
            .map(|(p, _)| {
                wrap::truncate(
                    vec![Span::styled(
                        format!(" ⏵ queued: {}", p.lines().next().unwrap_or("")),
                        self.theme.dim,
                    )],
                    area.width as usize,
                )
            })
            .collect();
        if let Some(s) = &self.search {
            let hits = self.hits();
            let pos =
                s.at.and_then(|a| hits.iter().position(|&h| h == a))
                    .map_or(0, |p| p + 1);
            let line = format!(
                " search: {}  {}/{}   Enter older · Shift+Enter newer · Esc close",
                s.query,
                pos,
                hits.len()
            );
            notes.push(wrap::truncate(
                vec![Span::styled(line, self.theme.warn)],
                area.width as usize,
            ));
        }
        if let Some(s) = self.editor.search_prompt() {
            notes.push(wrap::truncate(
                vec![Span::styled(format!(" {s}"), self.theme.warn)],
                area.width as usize,
            ));
        }
        let card_h = match (self.approvals.front(), self.asks.front()) {
            (Some(c), _) => c.height(area.width).min(area.height / 2),
            (None, Some(f)) => f.height(area.width).min(area.height * 2 / 3),
            _ => 0,
        };
        let comp_h = self
            .completion
            .as_ref()
            .map_or(0, |c| c.items.len().min(COMPLETE_ROWS) as u16);
        let widget_lines = |below: bool, me: &Self| -> Vec<Line<'static>> {
            me.widgets
                .values()
                .filter(|(_, b)| *b == below)
                .flat_map(|(l, _)| {
                    l.iter()
                        .map(|l| wrap::truncate(crate::panel::spans(l, &me.theme), area.width as usize))
                })
                .collect()
        };
        let (mut above_w, below_w) = (widget_lines(false, self), widget_lines(true, self));
        // Too narrow for the panel: open todos as a compact widget instead (pi's fallback).
        if f.area().width < crate::panel::MIN_SCREEN {
            above_w.extend(self.panel.todo_summary(area.width as usize, &self.theme));
        }
        let status_lines: Vec<Line<'static>> = match &self.footer {
            Some(lines) => lines
                .iter()
                .map(|l| wrap::truncate(crate::panel::spans(l, &self.theme), area.width as usize))
                .collect(),
            None => self.status_lines(area.width as usize),
        };
        let sh = status_lines.len() as u16;
        let above = self.init.statusline.above && self.footer.is_none();
        let [main, card, comp, pend, wa, st_above, ed, wb, st_below] = Layout::vertical([
            Constraint::Min(1),
            Constraint::Length(card_h),
            Constraint::Length(comp_h),
            Constraint::Length(notes.len() as u16),
            Constraint::Length(above_w.len() as u16),
            Constraint::Length(if above { sh } else { 0 }),
            Constraint::Length(ed_rows as u16 + 2),
            Constraint::Length(below_w.len() as u16),
            Constraint::Length(if above { 0 } else { sh }),
        ])
        .areas(area);
        let status = if above { st_above } else { st_below };
        f.render_widget(Paragraph::new(above_w), wa);
        f.render_widget(Paragraph::new(below_w), wb);
        let mut main = Rect {
            x: main.x + 1,
            width: main.width.saturating_sub(2),
            ..main
        };
        if let Some(h) = &self.header {
            let rows = (h.len() as u16).min(main.height / 3);
            let lines: Vec<Line> = h
                .iter()
                .take(rows as usize)
                .map(|l| wrap::truncate(crate::panel::spans(l, &self.theme), main.width as usize))
                .collect();
            f.render_widget(Paragraph::new(lines), Rect { height: rows, ..main });
            main = Rect {
                y: main.y + rows,
                height: main.height - rows,
                ..main
            };
        }
        self.draw_transcript(f, main);
        let t = &self.theme;
        f.render_widget(Paragraph::new(notes), pend);
        if let Some(c) = &self.completion {
            let first = (c.sel + 1).saturating_sub(COMPLETE_ROWS);
            let lines: Vec<Line> = c
                .items
                .iter()
                .enumerate()
                .skip(first)
                .take(COMPLETE_ROWS)
                .map(|(i, (label, _, detail))| {
                    let sel = i == c.sel;
                    wrap::truncate(
                        vec![
                            Span::styled(if sel { " ▸ " } else { "   " }, t.accent),
                            Span::styled(label.clone(), if sel { t.accent } else { t.text }),
                            Span::styled(format!("  {detail}"), t.dim),
                        ],
                        area.width as usize,
                    )
                })
                .collect();
            f.render_widget(Paragraph::new(lines), comp);
        }

        let (rows, (cr, cc)) = self.editor.view(inner_w, ed_rows);
        let mut block = Block::bordered()
            .padding(Padding::horizontal(1))
            .border_type(BorderType::Rounded)
            .border_style(match &self.editor_border {
                Some(b) => t.token(Some(b)),
                None => {
                    t.levels[ThinkingLevel::ALL
                        .iter()
                        .position(|l| *l == self.init.level)
                        .unwrap_or(3)]
                }
            });
        if let Some(title) = &self.editor_title {
            block = block.title(Span::styled(format!(" {title} "), t.dim));
        }
        let inner = block.inner(ed);
        let text: Vec<Line> = rows.into_iter().map(|r| Line::styled(r, t.text)).collect();
        f.render_widget(Paragraph::new(text).block(block), ed);
        if self.approvals.is_empty() && self.asks.is_empty() && self.overlay.is_none() {
            f.set_cursor_position(Position::new(inner.x + cc, inner.y + cr));
        }
        if let Some(c) = self.approvals.front() {
            c.render(f, card, t);
        } else if let Some(a) = self.asks.front() {
            a.render(f, card, t);
        }
        f.render_widget(Paragraph::new(status_lines), status);

        let t = &self.theme;
        match &self.overlay {
            Some(
                Overlay::Models(p)
                | Overlay::Sessions(p)
                | Overlay::Thinking(p)
                | Overlay::Branch(_, p)
                | Overlay::RewindMode(_, p),
            ) => p.render(f, dialog::centered(main, 70, 70, 20), t),
            Some(Overlay::Pager(p)) => p.render(f, dialog::centered(main, 90, 90, main.height), t),
            None => {}
        }
    }

    fn draw_transcript(&mut self, f: &mut Frame, area: Rect) {
        // An open image fits the transcript height, so it can always be seen whole.
        let fit = (area.height as usize).saturating_sub(2).max(2);
        if self.tr.image_rows != 0 && self.tr.image_rows != fit {
            self.tr.image_rows = fit;
            for t in &mut self.tr.turns {
                t.touch();
            }
        }
        let view = self.tr.layout(area.width as usize, &self.theme);
        let h = area.height as usize;
        let max_top = view.lines.len().saturating_sub(h);
        let top = match self.anchor {
            None => max_top,
            Some((turn, off)) => {
                let start = view.starts.get(turn).copied().unwrap_or(0);
                let next = view.starts.get(turn + 1).copied().unwrap_or(view.lines.len());
                (start + off.min(next.saturating_sub(start + 1))).min(max_top)
            }
        };
        if self.anchor.is_some() && top >= max_top {
            self.anchor = None;
        }
        let visible: Vec<Line> = view.lines[top..(top + h).min(view.lines.len())].to_vec();
        f.render_widget(Paragraph::new(visible), area);
        // Images only when wholly on screen; otherwise their placeholder line shows.
        #[cfg(feature = "images")]
        if let Some(imgs) = self.images.as_mut() {
            for (line, rows, img) in &view.images {
                if *line >= top && line + rows <= top + h {
                    let r = Rect {
                        x: area.x,
                        y: area.y + (line - top) as u16,
                        width: img.cols(area.width as usize) as u16,
                        height: *rows as u16,
                    };
                    f.render_widget(ratatui::widgets::Clear, Rect { width: area.width, ..r });
                    imgs.render(f, r, img);
                }
            }
        }
        if let Some(s) = self.search.as_ref().filter(|s| !s.query.is_empty()) {
            let q = s.query.to_lowercase();
            let buf = f.buffer_mut();
            for row in 0..area.height {
                let n = top + row as usize;
                let Some(line) = view.lines.get(n) else { break };
                let plain = wrap::plain(line).to_lowercase();
                let Some(byte) = plain.find(&q) else { continue };
                let col = plain[..byte].width() as u16;
                let st = if s.at == Some(n) {
                    self.theme.matched
                } else {
                    self.theme.selection
                };
                for c in col..(col + q.width() as u16).min(area.width) {
                    if let Some(cell) = buf.cell_mut(Position::new(area.x + c, area.y + row)) {
                        cell.set_style(st);
                    }
                }
            }
        }
        if let Some((a, b)) = self.selection {
            let (a, b) = (a.min(b), a.max(b));
            let buf = f.buffer_mut();
            for row in 0..area.height {
                let line = top + row as usize;
                if line < a.line || line > b.line {
                    continue;
                }
                for col in 0..area.width {
                    let c = col as usize;
                    let inside = (line > a.line || c >= a.col) && (line < b.line || c <= b.col);
                    if inside && let Some(cell) = buf.cell_mut(Position::new(area.x + col, area.y + row)) {
                        cell.set_style(self.theme.selection);
                    }
                }
            }
        }
        self.shown = Some(Shown { view, top, area });
    }

    /// Every status segment that has something to show (L9.12).
    fn status_lines(&mut self, width: usize) -> Vec<Line<'static>> {
        use crate::statusline::Value_;
        let mut v: std::collections::BTreeMap<String, Value_> = Default::default();
        v.insert(
            "activity".into(),
            Value_ {
                spans: self.activity_spans(),
                pct: None,
            },
        );
        let t = &self.theme;
        if !self.procs.is_empty() {
            v.insert("bg".into(), Value_::text(format!("⚙ {}", self.procs.len()), t.accent));
        }
        let agents = self.panel.live_subagents();
        if agents > 0 {
            v.insert("agents".into(), Value_::text(format!("agents {agents}"), t.accent));
        }
        if self.ctx > 0 {
            let pct = self.ctx as f64 * 100.0 / self.init.window_tokens.max(1) as f64;
            let st = match pct {
                p if p < 50.0 => t.ok,
                p if p < 70.0 => t.warn,
                p if p < 90.0 => t.code,
                _ => t.error,
            };
            v.insert(
                "ctx".into(),
                Value_ {
                    spans: vec![Span::styled(format!("ctx {pct:.0}%"), st)],
                    pct: Some(pct),
                },
            );
            if let Some(u) = self.usage.filter(|u| u.prompt_total() > 0) {
                let hit = u.cache_read as f64 * 100.0 / u.prompt_total() as f64;
                v.insert(
                    "cache".into(),
                    Value_ {
                        spans: vec![Span::styled(format!("cache {hit:.0}%"), t.dim)],
                        pct: Some(hit),
                    },
                );
            }
        }
        if self.init.price.is_some() || self.init.spent > 0.0 {
            let s = self.init.spent;
            v.insert(
                "cost".into(),
                Value_::text(
                    if s < 0.01 {
                        format!("${s:.4}")
                    } else {
                        format!("${s:.2}")
                    },
                    t.dim,
                ),
            );
        }
        v.insert(
            "model".into(),
            Value_ {
                spans: vec![
                    Span::styled(self.init.model.clone(), t.accent),
                    Span::styled(format!(" · {}", self.init.level.as_str()), t.dim),
                ],
                pct: None,
            },
        );
        v.insert("thinking".into(), Value_::text(self.init.level.as_str(), t.dim));
        let dir = std::path::Path::new(&self.init.cwd)
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        v.insert("cwd".into(), Value_::text(dir, t.dim));
        if let Some(b) = &self.init.branch {
            v.insert("git".into(), Value_::text(format!("⎇ {b}"), t.dim));
        }
        if let Some((_, wt)) = self.init.cwd.split_once("/.worktrees/") {
            v.insert(
                "wtpath".into(),
                Value_::text(format!("wt {}", wt.split('/').next().unwrap_or(wt)), t.warn),
            );
        }
        for (id, label, p) in [("usage_5h", "5h", self.quota.0), ("usage_week", "wk", self.quota.1)] {
            if let Some(p) = p {
                v.insert(
                    id.into(),
                    Value_ {
                        spans: vec![Span::styled(format!("{label} {p:.0}%"), t.dim)],
                        pct: Some(p),
                    },
                );
            }
        }
        if let Some(s) = self.task_started {
            v.insert(
                "turn_time".into(),
                Value_::text(crate::panel::secs_pub(s.elapsed()), t.dim),
            );
        }
        v.insert(
            "session_time".into(),
            Value_::text(crate::panel::secs_pub(self.started.elapsed()), t.dim),
        );
        if !self.pending.is_empty() {
            v.insert(
                "queue".into(),
                Value_::text(format!("queued {}", self.pending.len()), t.warn),
            );
        }
        if self.mcp.1 > 0 {
            let st = if self.mcp.0 < self.mcp.1 { t.warn } else { t.dim };
            v.insert(
                "mcp".into(),
                Value_::text(format!("mcp {}/{}", self.mcp.0, self.mcp.1), st),
            );
        }
        for (k, (text, style)) in &self.ext_status {
            v.insert(k.clone(), Value_::text(text.clone(), t.token(style.as_deref())));
        }
        let ext: Vec<String> = self.ext_status.keys().cloned().collect();
        crate::statusline::render(&self.init.statusline, &v, &ext, width, t)
    }

    /// The `activity` segment: what's going on, or what's wanted from the user.
    fn activity_spans(&mut self) -> Vec<Span<'static>> {
        let t = &self.theme;
        let mut left = Vec::new();
        if !self.approvals.is_empty() || !self.asks.is_empty() {
            left.push(Span::styled(" ? ", t.warn));
            left.push(Span::styled(
                if self.approvals.is_empty() {
                    "waiting for your answer"
                } else {
                    "waiting for your approval"
                },
                t.warn,
            ));
        } else if self.busy {
            left.push(Span::styled(
                format!(" {} ", SPINNER[self.spinner % SPINNER.len()]),
                t.accent,
            ));
            left.push(Span::styled(self.activity.clone(), t.text));
        } else {
            left.push(Span::styled(" ● ", t.dim));
            left.push(Span::styled("ready", t.dim));
        }
        if let Some((msg, at)) = &self.flash {
            if at.elapsed() < Duration::from_secs(3) {
                left.push(Span::styled(format!("  {msg}"), t.warn));
            } else {
                self.flash = None;
            }
        }
        if self.anchor.is_some() {
            left.push(Span::styled(
                format!("  ↓ scrolled ({})", self.keymap.keys(A::Bottom)),
                t.dim,
            ));
        }
        left
    }
}

/// Files and folders under `cwd` for `@` completion, respecting .gitignore.
fn list_files(cwd: &str) -> Vec<String> {
    let root = std::path::Path::new(cwd);
    let mut out: Vec<String> = ignore::WalkBuilder::new(root)
        .build()
        .flatten()
        .filter_map(|e| {
            let rel = e.path().strip_prefix(root).ok()?.to_string_lossy().into_owned();
            if rel.is_empty() {
                return None;
            }
            Some(if e.file_type()?.is_dir() {
                format!("{rel}/")
            } else {
                rel
            })
        })
        .take(FILE_CAP)
        .collect();
    out.sort();
    out
}

/// `@~/…`, `@/…`, `@../…`, `@./…`: list one folder, inside or outside the
/// project (pi style). `None` = not such a path; use the project-wide search.
fn browse(cwd: &str, q: &str) -> Option<Vec<(String, String, String)>> {
    let q = match q {
        "~" => "~/",
        ".." => "../",
        q if q.starts_with('/') || q.starts_with("~/") || q.starts_with("../") || q.starts_with("./") => q,
        _ => return None,
    };
    let (base, name) = q.split_at(q.rfind('/')? + 1);
    let dir = match base.strip_prefix("~/") {
        Some(rest) => std::path::PathBuf::from(std::env::var_os("HOME")?).join(rest),
        None => std::path::Path::new(cwd).join(base),
    };
    let name = name.to_lowercase();
    let mut hits: Vec<(bool, String)> = std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .filter_map(|e| {
            let n = e.file_name().to_string_lossy().into_owned();
            // Hidden entries only when asked for with a leading dot.
            let wanted = n.to_lowercase().starts_with(&name) && (!n.starts_with('.') || name.starts_with('.'));
            // `metadata` follows symlinks, so a link to a folder counts as a folder.
            wanted.then(|| (!std::fs::metadata(e.path()).is_ok_and(|m| m.is_dir()), n))
        })
        .collect();
    hits.sort(); // folders first, then by name
    Some(
        hits.into_iter()
            .take(200)
            .map(|(file, n)| {
                let tail = if file { " " } else { "/" };
                let label = if file { n.clone() } else { format!("{n}/") };
                (label, format!("@{base}{n}{tail}"), String::new())
            })
            .collect(),
    )
}
