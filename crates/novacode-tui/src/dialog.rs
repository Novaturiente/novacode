//! Overlays (L9.13): fuzzy picker, approval card (L6.1), keys help (L9.10)
//! and a pager for full tool output.

use crate::keys::{A, Keymap, Scope};
use crate::theme::Theme;
use crate::wrap;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, Paragraph};
use tokio::sync::oneshot;

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PickItem {
    pub label: String,
    pub detail: String,
    pub value: String,
}

pub enum Picked {
    Pending,
    Cancel,
    Value(String),
}

/// Fuzzy filter as you type; Enter picks.
pub struct Picker {
    pub title: String,
    items: Vec<PickItem>,
    filter: String,
    sel: usize,
}

/// Lower is better; `None` = not a subsequence.
pub fn fuzzy(hay: &str, needle: &str) -> Option<usize> {
    let hay = hay.to_lowercase();
    let mut score = 0;
    let mut last = None;
    let mut it = hay.char_indices();
    for n in needle.to_lowercase().chars() {
        let (i, _) = it.by_ref().find(|(_, c)| *c == n)?;
        score += last.map_or(i, |l: usize| i - l - 1);
        last = Some(i);
    }
    Some(score)
}

impl Picker {
    pub fn new(title: impl Into<String>, items: Vec<PickItem>) -> Self {
        Self {
            title: title.into(),
            items,
            filter: String::new(),
            sel: 0,
        }
    }

    /// Start on the item with this value (the current choice).
    pub fn select_value(mut self, value: &str) -> Self {
        self.sel = self.items.iter().position(|i| i.value == value).unwrap_or(0);
        self
    }

    fn visible(&self) -> Vec<&PickItem> {
        if self.filter.is_empty() {
            return self.items.iter().collect();
        }
        let mut v: Vec<(usize, &PickItem)> = self
            .items
            .iter()
            .filter_map(|i| fuzzy(&format!("{} {}", i.label, i.value), &self.filter).map(|s| (s, i)))
            .collect();
        v.sort_by_key(|(s, _)| *s);
        v.into_iter().map(|(_, i)| i).collect()
    }

    pub fn handle(&mut self, k: KeyEvent, km: &Keymap) -> Picked {
        let n = self.visible().len();
        match km.lookup(&k, &[Scope::Select]) {
            Some(A::SelUp) => self.sel = self.sel.saturating_sub(1),
            Some(A::SelDown) => self.sel = (self.sel + 1).min(n.saturating_sub(1)),
            Some(A::SelPageUp) => self.sel = self.sel.saturating_sub(10),
            Some(A::SelPageDown) => self.sel = (self.sel + 10).min(n.saturating_sub(1)),
            Some(A::SelCancel) => return Picked::Cancel,
            Some(A::SelConfirm) => {
                return match self.visible().get(self.sel) {
                    Some(i) => Picked::Value(i.value.clone()),
                    None => Picked::Pending,
                };
            }
            _ => match k.code {
                KeyCode::Backspace => {
                    self.filter.pop();
                    self.sel = 0;
                }
                KeyCode::Char(c) if !k.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) => {
                    self.filter.push(c);
                    self.sel = 0;
                }
                _ => {}
            },
        }
        Picked::Pending
    }

    pub fn render(&self, f: &mut Frame, area: Rect, t: &Theme) {
        let inner = frame(f, area, &self.title, t);
        let mut lines = vec![Line::from(vec![
            Span::styled("› ", t.accent),
            Span::styled(self.filter.clone(), t.text),
            Span::styled(if self.filter.is_empty() { "type to filter" } else { "" }, t.dim),
        ])];
        let vis = self.visible();
        let rows = inner.height.saturating_sub(1) as usize;
        let first = (self.sel + 1).saturating_sub(rows);
        for (i, item) in vis.iter().enumerate().skip(first).take(rows) {
            let selected = i == self.sel;
            let mark = if selected { "▸ " } else { "  " };
            let st = if selected { t.accent } else { t.text };
            lines.push(wrap::truncate(
                vec![
                    Span::styled(mark, t.accent),
                    Span::styled(item.label.clone(), st),
                    Span::styled(format!("  {}", item.detail), t.dim),
                ],
                inner.width as usize,
            ));
        }
        if vis.is_empty() {
            lines.push(Line::styled("  no match", t.dim));
        }
        f.render_widget(Paragraph::new(lines), inner);
        f.set_cursor_position((inner.x + 2 + self.filter.chars().count() as u16, inner.y));
    }
}

/// Clear `area`, draw a rounded frame with a title, return the inside.
pub fn frame(f: &mut Frame, area: Rect, title: &str, t: &Theme) -> Rect {
    f.render_widget(Clear, area);
    let b = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(t.accent)
        .padding(ratatui::widgets::Padding::horizontal(1))
        .title(Span::styled(format!(" {title} "), t.accent));
    let inner = b.inner(area);
    f.render_widget(b, area);
    inner
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Answer {
    Once,
    Session,
    Always,
    Deny(Option<String>),
}

/// A permission question from the policy (L6.1). The session waits on `reply`.
pub struct ApproveReq {
    /// Same id in every window; the first answer wins (L8.13).
    pub id: u64,
    pub tool: String,
    /// Full command or path.
    pub detail: String,
    pub reason: String,
    pub rule: String,
    /// Held by the risk gate: "always" only covers this exact command for the session.
    pub risky: bool,
    pub reply: oneshot::Sender<Answer>,
}

pub struct ApprovalCard {
    pub req: ApproveReq,
    /// Typing a reason after `n`.
    reason: Option<String>,
}

impl ApprovalCard {
    pub fn new(req: ApproveReq) -> Self {
        Self { req, reason: None }
    }

    /// `Some` = answered.
    pub fn handle(&mut self, k: KeyEvent) -> Option<Answer> {
        if let Some(r) = &mut self.reason {
            match k.code {
                KeyCode::Enter => return Some(Answer::Deny(Some(r.clone()).filter(|r| !r.trim().is_empty()))),
                KeyCode::Esc => return Some(Answer::Deny(None)),
                KeyCode::Backspace => {
                    r.pop();
                }
                KeyCode::Char(c) if !k.modifiers.contains(KeyModifiers::CONTROL) => r.push(c),
                _ => {}
            }
            return None;
        }
        match k.code {
            KeyCode::Char('y') | KeyCode::Enter => Some(Answer::Once),
            KeyCode::Char('a') => Some(Answer::Session),
            KeyCode::Char('A') if !self.req.risky => Some(Answer::Always),
            KeyCode::Char('n') => {
                self.reason = Some(String::new());
                None
            }
            KeyCode::Esc => Some(Answer::Deny(None)),
            KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => Some(Answer::Deny(None)),
            _ => None,
        }
    }

    pub fn height(&self, width: u16) -> u16 {
        self.lines(width.saturating_sub(4) as usize, &Theme::default(), true)
            .len() as u16
            + 2
    }

    /// Tool name, then the command or path under it, then why it is asked, then the keys.
    /// `spaced`: blank lines between those parts, dropped when the screen is short.
    fn lines(&self, w: usize, t: &Theme, spaced: bool) -> Vec<Line<'static>> {
        let r = &self.req;
        let bar = [Span::styled("│ ", t.dim)];
        // Line by line: `wrap::wrap` runs lines together, which glued multi-line commands.
        let mut body: Vec<Line<'static>> = r
            .detail
            .lines()
            .flat_map(|l| wrap::wrap(vec![Span::styled(wrap::sanitize(l), t.code)], w, &bar, &bar))
            .collect();
        if body.len() > 12 {
            let more = body.len() - 11;
            body.truncate(11);
            body.push(Line::styled(format!("│ … {more} more lines"), t.dim));
        }
        let mut lines = vec![Line::styled(r.tool.clone(), t.accent.add_modifier(Modifier::BOLD))];
        lines.extend(body);
        if spaced {
            lines.push(Line::default());
        }
        let why_st = if r.risky { t.error } else { t.dim };
        lines.extend(wrap::wrap(
            vec![Span::styled(r.reason.clone(), why_st)],
            w,
            &[Span::styled("why: ", t.dim)],
            &[Span::raw("     ")],
        ));
        if spaced {
            lines.push(Line::default());
        }
        match &self.reason {
            Some(reason) => lines.push(Line::from(vec![
                Span::styled("why not? ", t.warn),
                Span::styled(reason.clone(), t.text),
                Span::styled("  (Enter send, Esc skip)", t.dim),
            ])),
            None => {
                let mut keys = vec![
                    Span::styled("y", t.ok),
                    Span::styled(" once · ", t.dim),
                    Span::styled("a", t.ok),
                    Span::styled(" this session · ", t.dim),
                ];
                if !r.risky {
                    keys.push(Span::styled("A", t.ok));
                    keys.push(Span::styled(format!(" always ({}) · ", r.rule), t.dim));
                }
                keys.push(Span::styled("n", t.error));
                keys.push(Span::styled(" deny", t.dim));
                // Wrapped, not cut: a long rule must not hide `deny`.
                lines.extend(wrap::wrap(keys, w, &[], &[]));
            }
        }
        lines
    }

    pub fn render(&self, f: &mut Frame, area: Rect, t: &Theme) {
        let title = if self.req.risky { "Safety check" } else { "Allow?" };
        let inner = frame(f, area, title, t);
        let w = inner.width as usize;
        let mut lines = self.lines(w, t, true);
        if lines.len() > inner.height as usize {
            lines = self.lines(w, t, false);
        }
        f.render_widget(Paragraph::new(lines), inner);
    }
}

/// Scrollable text: full tool output, keys help.
pub struct Pager {
    pub title: String,
    lines: Vec<Line<'static>>,
    top: usize,
    filter: Option<String>,
    /// Typing filters lines (keys help).
    filterable: bool,
    all: Vec<Line<'static>>,
}

impl Pager {
    pub fn new(title: impl Into<String>, text: &str, t: &Theme) -> Self {
        let lines: Vec<Line<'static>> = wrap::sanitize(text)
            .lines()
            .map(|l| Line::styled(l.to_string(), t.text))
            .collect();
        Self {
            title: title.into(),
            all: lines.clone(),
            lines,
            top: 0,
            filter: None,
            filterable: false,
        }
    }

    pub fn keys_help(km: &Keymap, t: &Theme) -> Self {
        let mut all = Vec::new();
        for (scope, head) in [
            (Scope::App, "App"),
            (Scope::Scroll, "Transcript"),
            (Scope::Editor, "Editor"),
            (Scope::Select, "Lists"),
        ] {
            all.push(Line::styled(head, t.heading));
            for b in km.bindings.iter().filter(|b| b.def.scope == scope) {
                let keys = if b.keys.is_empty() {
                    "—".to_string()
                } else {
                    b.keys.join(", ")
                };
                let mut s = vec![
                    Span::styled(format!("  {keys:<24}"), t.accent),
                    Span::styled(b.def.desc, t.text),
                    Span::styled(format!("  {}", b.def.name), t.dim),
                ];
                if b.custom {
                    s.push(Span::styled("  (custom)", t.warn));
                }
                all.push(Line::from(s));
            }
            all.push(Line::default());
        }
        for c in km.conflicts() {
            all.push(Line::styled(format!("conflict: {c}"), t.error));
        }
        Self {
            title: "Keys — type to filter, Esc closes".into(),
            lines: all.clone(),
            all,
            top: 0,
            filter: Some(String::new()),
            filterable: true,
        }
    }

    fn refilter(&mut self) {
        let f = self.filter.clone().unwrap_or_default().to_lowercase();
        self.lines = self
            .all
            .iter()
            .filter(|l| f.is_empty() || wrap::plain(l).to_lowercase().contains(&f))
            .cloned()
            .collect();
        self.top = 0;
    }

    /// `true` = close.
    pub fn handle(&mut self, k: KeyEvent, page: usize) -> bool {
        let max = self.lines.len().saturating_sub(page);
        match k.code {
            KeyCode::Esc => return true,
            KeyCode::Char('q') if !self.filterable => return true,
            KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => return true,
            KeyCode::Up => self.top = self.top.saturating_sub(1),
            KeyCode::Down => self.top = (self.top + 1).min(max),
            KeyCode::PageUp => self.top = self.top.saturating_sub(page),
            KeyCode::PageDown | KeyCode::Char(' ') if !self.filterable || k.code == KeyCode::PageDown => {
                self.top = (self.top + page).min(max)
            }
            KeyCode::Home => self.top = 0,
            KeyCode::End => self.top = max,
            KeyCode::Char('g') if !self.filterable => self.top = 0,
            KeyCode::Char('G') if !self.filterable => self.top = max,
            KeyCode::Backspace if self.filterable => {
                if let Some(f) = &mut self.filter {
                    f.pop();
                }
                self.refilter();
            }
            KeyCode::Char(c) if self.filterable && !k.modifiers.contains(KeyModifiers::CONTROL) => {
                if let Some(f) = &mut self.filter {
                    f.push(c);
                }
                self.refilter();
            }
            _ => {}
        }
        false
    }

    pub fn render(&self, f: &mut Frame, area: Rect, t: &Theme) {
        let mut title = self.title.clone();
        if let Some(q) = self.filter.as_ref().filter(|q| !q.is_empty()) {
            title = format!("{title} · /{q}");
        }
        let inner = frame(f, area, &title, t);
        let h = inner.height as usize;
        let shown: Vec<Line> = self
            .lines
            .iter()
            .skip(self.top)
            .take(h)
            .map(|l| wrap::truncate(l.spans.clone(), inner.width as usize))
            .collect();
        f.render_widget(Paragraph::new(shown), inner);
    }
}

/// A centred box `pw`% wide and at most `ph`% high.
pub fn centered(area: Rect, pw: u16, ph: u16, want_h: u16) -> Rect {
    let w = (area.width * pw / 100).max(20).min(area.width);
    let h = want_h.min(area.height * ph / 100).max(5).min(area.height);
    Rect {
        x: area.x + (area.width - w) / 2,
        y: area.y + (area.height - h) / 2,
        width: w,
        height: h,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn k(c: KeyCode) -> KeyEvent {
        KeyEvent::new(c, KeyModifiers::NONE)
    }

    #[test]
    fn approval_card_keeps_command_lines() {
        let (reply, _rx) = tokio::sync::oneshot::channel();
        let card = ApprovalCard::new(ApproveReq {
            id: 1,
            tool: "bash".into(),
            detail: "python3 -c \"from inventory import Inventory as I\ni=I()\"".into(),
            reason: "r".into(),
            rule: String::new(),
            risky: false,
            reply,
        });
        let text: Vec<String> = card
            .lines(80, &Theme::default(), true)
            .iter()
            .map(crate::wrap::plain)
            .collect();
        assert!(
            text[0] == "bash" && text[1].ends_with("Inventory as I") && text[2] == "│ i=I()\"",
            "{text:?}"
        );
        assert_eq!(text[4], "why: r");
    }

    #[test]
    fn picker_filters_and_picks() {
        let items = ["claude-opus-5-5", "claude-sonnet-4-5", "claude-haiku-4-5"]
            .map(|m| PickItem {
                label: m.into(),
                detail: String::new(),
                value: format!("claude/{m}"),
            })
            .to_vec();
        let km = Keymap::default();
        let mut p = Picker::new("Model", items);
        for c in "hku".chars() {
            p.handle(k(KeyCode::Char(c)), &km);
        }
        assert!(matches!(p.handle(k(KeyCode::Enter), &km), Picked::Value(v) if v == "claude/claude-haiku-4-5"));
        assert!(matches!(p.handle(k(KeyCode::Esc), &km), Picked::Cancel));
        assert_eq!(fuzzy("claude-opus", "cop"), Some(6));
        assert_eq!(fuzzy("abc", "x"), None);
    }

    #[test]
    fn approval_keys() {
        let card = |risky| {
            let (tx, _rx) = oneshot::channel();
            ApprovalCard::new(ApproveReq {
                id: 1,
                tool: "bash".into(),
                detail: "rm -rf build".into(),
                reason: "deletes files".into(),
                rule: "Bash(rm -rf:*)".into(),
                risky,
                reply: tx,
            })
        };
        assert_eq!(card(false).handle(k(KeyCode::Char('A'))), Some(Answer::Always));
        // "Always" isn't offered for risk-gated commands.
        assert_eq!(card(true).handle(k(KeyCode::Char('A'))), None);
        let mut c = card(false);
        assert_eq!(c.handle(k(KeyCode::Char('n'))), None);
        for ch in "use cargo clean".chars() {
            c.handle(k(KeyCode::Char(ch)));
        }
        assert_eq!(
            c.handle(k(KeyCode::Enter)),
            Some(Answer::Deny(Some("use cargo clean".into())))
        );
    }
}
