//! App state, input handling and drawing. Talks to the session only through
//! [`AgentEvent`]s in and [`Cmd`]s out, so the same UI can later sit on an
//! attach socket (L8.13).

use crate::editor::{Action, Editor};
use crate::theme::Theme;
use crate::transcript::{Transcript, View};
use crate::wrap;
use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use novacode_types::{AgentEvent, BlockKind, Message, StopReason, StreamEvent, Usage};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Paragraph};
use std::time::{Duration, Instant};
use tokio::sync::mpsc::UnboundedSender;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

const EDITOR_MAX_ROWS: usize = 10;
const WHEEL: isize = 3;
const DOUBLE_PRESS: Duration = Duration::from_millis(500);
const SPINNER: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// What the UI knows at start.
pub struct Init {
    /// `provider/model`.
    pub model: String,
    pub cwd: String,
    pub branch: Option<String>,
    pub window_tokens: u64,
    /// Messages of a resumed session.
    pub history: Vec<Message>,
}

/// What the UI asks of the session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cmd {
    Prompt(String),
    /// Delivered before the next model call of the running turn (L3.7).
    Steer(String),
    Abort,
    Quit,
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

pub struct App {
    pub theme: Theme,
    pub tr: Transcript,
    pub editor: Editor,
    tx: UnboundedSender<Cmd>,
    init: Init,
    pub busy: bool,
    activity: String,
    pub spinner: usize,
    usage: Option<Usage>,
    /// Steering messages sent but not yet delivered.
    pending: Vec<String>,
    /// `None` = follow the tail; otherwise (turn, line offset in turn) at the top.
    anchor: Option<(usize, usize)>,
    shown: Option<Shown>,
    selection: Option<(Pos, Pos)>,
    flash: Option<(String, Instant)>,
    last_ctrl_c: Option<Instant>,
    last_esc: Option<Instant>,
    editor_width: usize,
    pub dirty: bool,
    pub quit: bool,
    pub suspend: bool,
    pub clear: bool,
}

impl App {
    pub fn new(mut init: Init, tx: UnboundedSender<Cmd>) -> Self {
        let mut tr = Transcript::default();
        let mut history = Vec::new();
        let mut usage = None;
        for m in std::mem::take(&mut init.history) {
            if let Message::User { content } = &m {
                history.extend(content.iter().find_map(|b| match b {
                    novacode_types::Block::Text { text, .. } => Some(text.clone()),
                    _ => None,
                }));
            }
            if let Message::Assistant(a) = &m {
                usage = Some(a.usage);
            }
            tr.on_message(&m);
        }
        tr.settle();
        Self {
            theme: Theme::default(),
            tr,
            editor: Editor::with_history(history),
            tx,
            init,
            busy: false,
            activity: String::new(),
            spinner: 0,
            usage,
            pending: Vec::new(),
            anchor: None,
            shown: None,
            selection: None,
            flash: None,
            last_ctrl_c: None,
            last_esc: None,
            editor_width: 80,
            dirty: true,
            quit: false,
            suspend: false,
            clear: false,
        }
    }

    fn send(&mut self, c: Cmd) {
        if self.tx.send(c).is_err() {
            self.quit = true;
        }
    }

    fn flash(&mut self, s: impl Into<String>) {
        self.flash = Some((s.into(), Instant::now()));
    }

    pub fn on_agent(&mut self, ev: AgentEvent) {
        self.dirty = true;
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
                self.tr.on_stream(&e);
            }
            AgentEvent::Message(m) => {
                if let Message::User { content } = &m {
                    let text: String = content
                        .iter()
                        .filter_map(|b| match b {
                            novacode_types::Block::Text { text, .. } => Some(text.as_str()),
                            _ => None,
                        })
                        .collect();
                    if let Some(i) = self.pending.iter().position(|p| *p == text) {
                        self.pending.remove(i);
                    }
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
            AgentEvent::Settled => {
                self.busy = false;
                self.activity.clear();
                self.tr.settle();
            }
        }
    }

    pub fn on_event(&mut self, ev: Event) {
        self.dirty = true;
        match ev {
            Event::Key(k) if k.kind != KeyEventKind::Release => self.on_key(k),
            Event::Paste(s) => self.editor.paste(&s),
            Event::Mouse(m) => self.on_mouse(m),
            _ => {}
        }
    }

    pub fn on_key(&mut self, k: KeyEvent) {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        let alt = k.modifiers.contains(KeyModifiers::ALT);
        let now = Instant::now();
        self.selection = None;
        match k.code {
            KeyCode::Char('c') if ctrl => {
                if !self.editor.is_empty() {
                    self.editor.clear();
                } else if self.busy {
                    self.send(Cmd::Abort);
                } else if self.last_ctrl_c.is_some_and(|t| now - t < Duration::from_secs(1)) {
                    self.quit = true;
                } else {
                    self.flash("press Ctrl+C again to quit");
                }
                self.last_ctrl_c = Some(now);
            }
            KeyCode::Char('d') if ctrl && self.editor.is_empty() => self.quit = true,
            KeyCode::Char('z') if ctrl => self.suspend = true,
            KeyCode::Char('l') if ctrl => self.clear = true,
            KeyCode::Char('o') if alt => self.tr.toggle_all(),
            KeyCode::Esc => {
                if self.busy {
                    self.send(Cmd::Abort);
                } else if self.last_esc.is_some_and(|t| now - t < DOUBLE_PRESS) {
                    self.editor.clear();
                }
                self.last_esc = Some(now);
            }
            KeyCode::PageUp => self.scroll_by(-(self.page() as isize)),
            KeyCode::PageDown => self.scroll_by(self.page() as isize),
            KeyCode::Home if ctrl => self.scroll_by(isize::MIN / 2),
            KeyCode::End if ctrl => self.anchor = None,
            _ => match self.editor.handle_key(k, self.editor_width) {
                Action::None => {}
                Action::Submit(text) => self.submit(text, true),
                Action::FollowUp(text) => self.submit(text, false),
            },
        }
    }

    fn submit(&mut self, text: String, steer: bool) {
        match text.trim() {
            "/quit" | "/exit" => {
                self.quit = true;
                return;
            }
            _ => {}
        }
        self.anchor = None;
        if self.busy {
            self.pending.push(text.clone());
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
                    if let Some(t) = toggle {
                        self.tr.toggle(t);
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
        let area = f.area();
        let inner_w = area.width.saturating_sub(2) as usize;
        self.editor_width = inner_w;
        let ed_rows = self.editor.height(inner_w).clamp(1, EDITOR_MAX_ROWS);
        let pending = self.pending.len().min(3) as u16;
        let [main, pend, ed, status] = Layout::vertical([
            Constraint::Min(1),
            Constraint::Length(pending),
            Constraint::Length(ed_rows as u16 + 2),
            Constraint::Length(1),
        ])
        .areas(area);
        let main = Rect {
            x: main.x + 1,
            width: main.width.saturating_sub(2),
            ..main
        };
        self.draw_transcript(f, main);
        let t = &self.theme;

        let lines: Vec<Line> = self
            .pending
            .iter()
            .rev()
            .take(3)
            .rev()
            .map(|p| {
                wrap::truncate(
                    vec![Span::styled(
                        format!(" ⏵ queued: {}", p.lines().next().unwrap_or("")),
                        t.dim,
                    )],
                    area.width as usize,
                )
            })
            .collect();
        f.render_widget(Paragraph::new(lines), pend);

        let (rows, (cr, cc)) = self.editor.view(inner_w, ed_rows);
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(t.border);
        let inner = block.inner(ed);
        let text: Vec<Line> = rows.into_iter().map(|r| Line::styled(r, t.text)).collect();
        f.render_widget(Paragraph::new(text).block(block), ed);
        f.set_cursor_position(Position::new(inner.x + cc, inner.y + cr));

        let st = self.status_line(area.width as usize);
        f.render_widget(Paragraph::new(st), status);
    }

    fn draw_transcript(&mut self, f: &mut Frame, area: Rect) {
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

    fn status_line(&mut self, width: usize) -> Line<'static> {
        let t = &self.theme;
        let mut left = Vec::new();
        if self.busy {
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
            left.push(Span::styled("  ↓ scrolled (Ctrl+End)", t.dim));
        }
        let mut right = Vec::new();
        if let Some(u) = self.usage {
            let used = u.prompt_total() + u.output;
            let pct = used as f64 * 100.0 / self.init.window_tokens.max(1) as f64;
            let st = match pct {
                p if p < 50.0 => t.ok,
                p if p < 70.0 => t.warn,
                p if p < 90.0 => t.code,
                _ => t.error,
            };
            right.push(Span::styled(format!("ctx {pct:.0}%"), st));
            if u.prompt_total() > 0 {
                let hit = u.cache_read as f64 * 100.0 / u.prompt_total() as f64;
                right.push(Span::styled(format!("  cache {hit:.0}%"), t.dim));
            }
            right.push(Span::raw("  "));
        }
        right.push(Span::styled(self.init.model.clone(), t.accent));
        let dir = std::path::Path::new(&self.init.cwd)
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        right.push(Span::styled(format!("  {dir}"), t.dim));
        if let Some(b) = &self.init.branch {
            right.push(Span::styled(format!(" ⎇ {b}"), t.dim));
        }
        right.push(Span::raw(" "));
        // Narrow screen: keep the activity, drop folder and branch first, then the model.
        while wrap::width(&right) + 12 > width && right.len() > 1 {
            right.remove(right.len().saturating_sub(2));
        }
        let rw = wrap::width(&right);
        let left = wrap::truncate(left, width.saturating_sub(rw + 1));
        let pad = width.saturating_sub(wrap::width(&left.spans) + rw);
        let mut spans = left.spans;
        spans.push(Span::raw(" ".repeat(pad)));
        spans.extend(right);
        Line::from(spans)
    }
}
