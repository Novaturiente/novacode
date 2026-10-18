//! The conversation as turns of items, laid out into visual lines (L9.1, L9.7,
//! L9.8). A finished turn is laid out once per width and cached; only the
//! turn still running is laid out again. Turn-collapse is built in.

use crate::markdown;
use crate::theme::Theme;
use crate::wrap::{self, sanitize};
use novacode_types::{Block, BlockKind, Message, StreamEvent};
use ratatui::text::{Line, Span};
use serde_json::Value;

const OUTPUT_TAIL: usize = 4;
const LIVE_CAP: usize = 64 * 1024;

pub enum Item {
    User(String),
    /// Text and thinking of one assistant message.
    Assistant(Vec<(BlockKind, String)>),
    Tool(Tool),
    Notice {
        text: String,
        error: bool,
    },
}

pub struct Tool {
    pub id: String,
    pub name: String,
    pub args: Value,
    pub output: String,
    pub is_error: bool,
    pub done: bool,
    pub duration_ms: Option<u64>,
}

#[derive(Default)]
pub struct Turn {
    pub items: Vec<Item>,
    pub done: bool,
    pub expanded: bool,
    cache: Option<Layout>,
}

struct Layout {
    width: usize,
    expanded: bool,
    lines: Vec<Line<'static>>,
    /// Line offsets (inside this turn) that toggle collapse on click.
    toggles: Vec<usize>,
}

impl Turn {
    fn tool_count(&self) -> (usize, usize) {
        let tools = self.items.iter().filter_map(|i| match i {
            Item::Tool(t) => Some(t),
            _ => None,
        });
        tools.fold((0, 0), |(n, e), t| (n + 1, e + t.is_error as usize))
    }

    fn collapsed(&self) -> bool {
        self.done && !self.expanded && self.tool_count().0 > 0
    }

    pub fn touch(&mut self) {
        self.cache = None;
    }
}

/// Everything shown in the transcript area.
#[derive(Default)]
pub struct Transcript {
    pub turns: Vec<Turn>,
    /// The assistant message being streamed.
    pub live: Option<Vec<(BlockKind, String)>>,
    /// Default for finished turns; Alt+O flips it.
    pub expand_all: bool,
    pub thinking: Thinking,
    /// Key that shows thinking in full, named on folded thinking lines.
    pub thinking_hint: String,
    /// Stream pacer (L9.7): bytes of each live block shown so far.
    pub(crate) live_shown: Vec<usize>,
}

/// How thinking blocks show (L9.7, setting `ui.thinking`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Thinking {
    /// One line with a word count.
    #[default]
    Collapsed,
    Full,
    Hidden,
}

impl Thinking {
    pub fn parse(s: &str) -> Self {
        match s {
            "full" => Self::Full,
            "hidden" => Self::Hidden,
            _ => Self::Collapsed,
        }
    }
}

/// The laid-out transcript for one frame.
pub struct View {
    pub lines: Vec<Line<'static>>,
    /// Where each turn starts in `lines`.
    pub starts: Vec<usize>,
    /// (line, turn) pairs a click toggles.
    pub toggles: Vec<(usize, usize)>,
}

impl Transcript {
    fn turn(&mut self) -> &mut Turn {
        if self.turns.is_empty() {
            self.turns.push(Turn::default());
        }
        let t = self.turns.last_mut().unwrap();
        t.touch();
        t
    }

    fn tool(&mut self, id: &str) -> Option<&mut Tool> {
        for turn in self.turns.iter_mut().rev() {
            for item in turn.items.iter_mut().rev() {
                if let Item::Tool(t) = item
                    && t.id == id
                {
                    turn.cache = None;
                    return Some(t);
                }
            }
        }
        None
    }

    pub fn notice(&mut self, text: impl Into<String>, error: bool) {
        self.turn().items.push(Item::Notice {
            text: text.into(),
            error,
        });
    }

    /// Mark every turn finished (the agent stopped).
    pub fn settle(&mut self) {
        for t in &mut self.turns {
            if !t.done {
                t.done = true;
                t.expanded = self.expand_all;
                t.touch();
            }
        }
    }

    pub fn toggle_all(&mut self) {
        self.expand_all = !self.expand_all;
        for t in self.turns.iter_mut().filter(|t| t.done) {
            t.expanded = self.expand_all;
            t.touch();
        }
    }

    pub fn toggle(&mut self, turn: usize) {
        if let Some(t) = self.turns.get_mut(turn) {
            t.expanded = !t.expanded;
            t.touch();
        }
    }

    fn items(&self) -> impl Iterator<Item = &Item> {
        self.turns.iter().rev().flat_map(|t| t.items.iter().rev())
    }

    /// The newest tool call: (title, full output), for the pager.
    pub fn last_tool(&self) -> Option<(String, String)> {
        self.items().find_map(|i| match i {
            Item::Tool(t) => Some((format!("{} {}", t.name, summary(&t.name, &t.args)), t.output.clone())),
            _ => None,
        })
    }

    /// Text of the newest assistant reply.
    pub fn last_reply(&self) -> Option<String> {
        self.items().find_map(|i| match i {
            Item::Assistant(b) => {
                let text: Vec<&str> = b
                    .iter()
                    .filter(|(k, _)| *k == BlockKind::Text)
                    .map(|(_, t)| t.as_str())
                    .collect();
                (!text.is_empty()).then(|| text.join("\n\n"))
            }
            _ => None,
        })
    }

    pub fn on_stream(&mut self, ev: &StreamEvent) {
        let live = self.live.get_or_insert_with(Vec::new);
        match ev {
            StreamEvent::BlockStart { index, kind, .. } => {
                while live.len() <= *index {
                    live.push((BlockKind::Text, String::new()));
                }
                live[*index].0 = *kind;
            }
            StreamEvent::Delta { index, text } => {
                if let Some(b) = live.get_mut(*index)
                    && b.1.len() < LIVE_CAP * 16
                {
                    b.1.push_str(text);
                }
            }
            StreamEvent::RetryRollback => {
                self.live = None;
                self.live_shown.clear();
            }
            _ => {}
        }
    }

    pub fn on_message(&mut self, m: &Message) {
        match m {
            Message::User { content } => {
                let text = text_of(content);
                for t in &mut self.turns {
                    if !t.done {
                        t.done = true;
                        t.expanded = self.expand_all;
                        t.touch();
                    }
                }
                self.turns.push(Turn {
                    items: vec![Item::User(text)],
                    ..Default::default()
                });
            }
            Message::Assistant(a) => {
                self.live = None;
                self.live_shown.clear();
                let blocks: Vec<(BlockKind, String)> = a
                    .content
                    .iter()
                    .filter_map(|b| match b {
                        Block::Text { text, .. } if !text.trim().is_empty() => Some((BlockKind::Text, text.clone())),
                        Block::Thinking { text, .. } if !text.trim().is_empty() => {
                            Some((BlockKind::Thinking, text.clone()))
                        }
                        _ => None,
                    })
                    .collect();
                let turn = self.turn();
                if !blocks.is_empty() {
                    turn.items.push(Item::Assistant(blocks));
                }
                for c in a.tool_calls() {
                    turn.items.push(Item::Tool(Tool {
                        id: c.id.clone(),
                        name: c.name.clone(),
                        args: c.arguments.clone(),
                        output: String::new(),
                        is_error: false,
                        done: false,
                        duration_ms: None,
                    }));
                }
                if let Some(e) = &a.error_message {
                    turn.items.push(Item::Notice {
                        text: e.clone(),
                        error: true,
                    });
                }
            }
            Message::ToolResult {
                call_id,
                content,
                is_error,
                ..
            } => {
                if let Some(t) = self.tool(call_id) {
                    t.output = text_of(content);
                    t.is_error = *is_error;
                    t.done = true;
                }
            }
            Message::System { .. } | Message::Custom { .. } => {}
        }
    }

    pub fn tool_update(&mut self, id: &str, text: &str) {
        if let Some(t) = self.tool(id) {
            t.output.push_str(text);
            if t.output.len() > LIVE_CAP {
                let mut cut = t.output.len() - LIVE_CAP;
                while !t.output.is_char_boundary(cut) {
                    cut += 1;
                }
                t.output.drain(..cut);
            }
        }
    }

    pub fn tool_end(&mut self, id: &str, is_error: bool, ms: u64) {
        if let Some(t) = self.tool(id) {
            t.duration_ms = Some(ms);
            t.is_error |= is_error;
        }
    }

    pub fn set_thinking(&mut self, th: Thinking) {
        self.thinking = th;
        for t in &mut self.turns {
            t.touch();
        }
    }

    /// Reveal a bit more of the live text; `true` while some is still held back.
    /// The step grows with the backlog, so the display never falls far behind.
    pub fn pace(&mut self) -> bool {
        let Some(live) = &self.live else { return false };
        self.live_shown.resize(live.len(), 0);
        let mut behind = false;
        for (i, (kind, text)) in live.iter().enumerate() {
            let shown = &mut self.live_shown[i];
            if *kind != BlockKind::Text {
                *shown = text.len();
                continue;
            }
            let backlog = text.len() - (*shown).min(text.len());
            let mut next = (*shown + (backlog / 8).max(3)).min(text.len());
            while !text.is_char_boundary(next) {
                next += 1;
            }
            *shown = next;
            behind |= next < text.len();
        }
        behind
    }

    fn live_view(&self) -> Option<Vec<(BlockKind, String)>> {
        let live = self.live.as_ref()?;
        Some(
            live.iter()
                .enumerate()
                .map(|(i, (k, t))| {
                    // Only text is paced; thinking shows its word count straight away.
                    let n = match k {
                        BlockKind::Text => self.live_shown.get(i).copied().unwrap_or(0).min(t.len()),
                        _ => t.len(),
                    };
                    (*k, t[..n].to_string())
                })
                .collect(),
        )
    }

    /// Lay out every turn for `width`, reusing cached finished turns.
    pub fn layout(&mut self, width: usize, theme: &Theme) -> View {
        let thinking = self.thinking;
        let hint = self.thinking_hint.clone();
        let live = self.live_view();
        let mut v = View {
            lines: Vec::new(),
            starts: Vec::new(),
            toggles: Vec::new(),
        };
        let last = self.turns.len().saturating_sub(1);
        for (i, turn) in self.turns.iter_mut().enumerate() {
            let fresh = turn
                .cache
                .as_ref()
                .is_some_and(|c| c.width == width && c.expanded == turn.expanded);
            if !fresh {
                turn.cache = Some(lay_turn(turn, width, theme, thinking, &self.thinking_hint));
            }
            let c = turn.cache.as_ref().unwrap();
            if i > 0 {
                v.lines.push(Line::default());
            }
            v.starts.push(v.lines.len());
            v.toggles.extend(c.toggles.iter().map(|o| (v.lines.len() + o, i)));
            v.lines.extend(c.lines.iter().cloned());
            if i == last
                && let Some(live) = &live
            {
                v.lines.extend(lay_assistant(live, width, theme, true, thinking, &hint));
            }
        }
        if self.turns.is_empty()
            && let Some(live) = &live
        {
            v.lines.extend(lay_assistant(live, width, theme, true, thinking, &hint));
        }
        v
    }
}

fn text_of(content: &[Block]) -> String {
    content
        .iter()
        .filter_map(|b| match b {
            Block::Text { text, .. } => Some(text.as_str()),
            Block::Image { .. } => Some("[image]"),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn lay_turn(turn: &Turn, width: usize, t: &Theme, thinking: Thinking, hint: &str) -> Layout {
    let mut lines = Vec::new();
    let mut toggles = Vec::new();
    let collapsed = turn.collapsed();
    let (n, errors) = turn.tool_count();
    // In a collapsed turn only the prompt, the fold line and the final reply show.
    let final_reply = turn
        .items
        .iter()
        .rposition(|i| matches!(i, Item::Assistant(b) if b.iter().any(|(k, _)| *k == BlockKind::Text)));
    let mut folded = false;
    for (idx, item) in turn.items.iter().enumerate() {
        // Notices (errors, "interrupted", retries) always show.
        let is_step = !matches!(item, Item::User(_) | Item::Notice { .. }) && Some(idx) != final_reply;
        if turn.done && n > 0 && !folded && is_step {
            folded = true;
            toggles.push(lines.len());
            let arrow = if collapsed { "▸" } else { "▾" };
            let mut s = vec![Span::styled(
                format!("{arrow} {n} tool call{}", if n == 1 { "" } else { "s" }),
                t.toggle,
            )];
            if errors > 0 {
                s.push(Span::styled(format!(" · {errors} failed"), t.error));
            }
            lines.push(Line::from(s));
        }
        if collapsed && is_step {
            continue;
        }
        match item {
            Item::User(text) => {
                if !lines.is_empty() || !matches!(turn.items.first(), Some(Item::User(_))) {
                    lines.push(Line::default());
                }
                lines.extend(user_box(text, width, t));
            }
            Item::Assistant(blocks) => {
                let only_reply = collapsed && Some(idx) == final_reply;
                let blocks: Vec<_> = blocks
                    .iter()
                    .filter(|(k, _)| !only_reply || *k == BlockKind::Text)
                    .cloned()
                    .collect();
                lines.extend(lay_assistant(&blocks, width, t, false, thinking, hint));
            }
            Item::Tool(tool) => lines.extend(lay_tool(tool, width, t)),
            Item::Notice { text, error } => {
                let st = if *error { t.error } else { t.dim };
                lines.extend(wrap::wrap(
                    vec![Span::styled(sanitize(text), st)],
                    width,
                    &[Span::styled(if *error { "! " } else { "· " }, st)],
                    &[Span::raw("  ")],
                ));
            }
        }
    }
    Layout {
        width,
        expanded: turn.expanded,
        lines,
        toggles,
    }
}

/// A prompt in a heavy box, so the start of each turn stands out when scrolling
/// (like the user's pi `rounded-boxes` extension).
fn user_box(text: &str, width: usize, t: &Theme) -> Vec<Line<'static>> {
    let b = &t.user_box;
    let edge = if b.border.is_some() { 1 } else { 0 };
    let inner = width.saturating_sub(2 * (edge + b.pad_x)).max(1);
    // The text colour's background (if any) fills the box, padding included.
    let fill = ratatui::style::Style {
        fg: None,
        add_modifier: ratatui::style::Modifier::empty(),
        ..b.text
    };
    let mut rows = vec![Vec::new(); b.pad_y];
    for para in split_lines(vec![Span::styled(sanitize(text), b.text)]) {
        rows.extend(wrap::wrap(para, inner, &[], &[]).into_iter().map(|l| l.spans));
    }
    rows.extend(vec![Vec::new(); b.pad_y]);
    let side = |s: &str| Span::styled(s.to_string(), b.border_style);
    let mut out = Vec::new();
    if let Some([tl, tr, _, _, h, _]) = b.border {
        let rule: String = std::iter::repeat_n(h, width.saturating_sub(2)).collect();
        out.push(Line::styled(format!("{tl}{rule}{tr}"), b.border_style));
    }
    for row in rows {
        let w: usize = row.iter().map(|s| s.width()).sum();
        let mut spans = Vec::new();
        if let Some(g) = b.border {
            spans.push(side(&g[5].to_string()));
        }
        spans.push(Span::styled(" ".repeat(b.pad_x), fill));
        spans.extend(row);
        spans.push(Span::styled(" ".repeat(inner.saturating_sub(w) + b.pad_x), fill));
        if let Some(g) = b.border {
            spans.push(side(&g[5].to_string()));
        }
        out.push(Line::from(spans));
    }
    if let Some([_, _, bl, br, h, _]) = b.border {
        let rule: String = std::iter::repeat_n(h, width.saturating_sub(2)).collect();
        out.push(Line::styled(format!("{bl}{rule}{br}"), b.border_style));
    }
    out
}

fn split_lines(spans: Vec<Span<'static>>) -> Vec<Vec<Span<'static>>> {
    let mut out = vec![Vec::new()];
    for s in spans {
        for (i, part) in s.content.split('\n').enumerate() {
            if i > 0 {
                out.push(Vec::new());
            }
            out.last_mut().unwrap().push(Span::styled(part.to_string(), s.style));
        }
    }
    out
}

fn lay_assistant(
    blocks: &[(BlockKind, String)],
    width: usize,
    t: &Theme,
    live: bool,
    thinking: Thinking,
    hint: &str,
) -> Vec<Line<'static>> {
    let mut lines = vec![Line::default()];
    for (kind, text) in blocks {
        match kind {
            BlockKind::Thinking if thinking == Thinking::Hidden => {}
            BlockKind::Thinking if thinking == Thinking::Full => {
                lines.extend(wrap::wrap(
                    vec![Span::styled(sanitize(text.trim()), t.thinking)],
                    width,
                    &[Span::styled("∴ ", t.thinking)],
                    &[Span::raw("  ")],
                ));
            }
            // L9.7: by default thinking shows as one collapsed line.
            BlockKind::Thinking => {
                let words = text.split_whitespace().count();
                let label = if live { "thinking…" } else { "thought" };
                let mut l = Line::styled(format!("∴ {label} · {words} words"), t.thinking);
                if !hint.is_empty() {
                    l.push_span(Span::styled(format!(" · {hint} to show"), t.dim));
                }
                lines.push(l);
            }
            BlockKind::Text => lines.extend(markdown::render(text, width, t, !live)),
            _ => {}
        }
    }
    if lines.len() == 1 {
        lines.clear();
    }
    lines
}

/// One-line summary of a call's arguments for the header.
pub fn summary(name: &str, args: &Value) -> String {
    let s = |k: &str| args.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    let first_line = |v: String| v.lines().next().unwrap_or("").to_string();
    match name {
        "bash" => format!("$ {}", first_line(s("command"))),
        "read" | "write" | "edit" | "ls" => s("path"),
        "grep" => {
            let p = s("path");
            if p.is_empty() {
                s("pattern")
            } else {
                format!("{}  in {p}", s("pattern"))
            }
        }
        "find" => s("pattern"),
        "apply_patch" => "patch".into(),
        _ => {
            let j = args.to_string();
            if j == "{}" { String::new() } else { j }
        }
    }
}

/// `+`/`-` for unified-diff lines and the edit tool's `42+ new` / `42- old` lines.
fn diff_sign(l: &str) -> Option<char> {
    let rest = l.trim_start_matches(|c: char| c.is_ascii_digit());
    let numbered = rest.len() < l.len();
    match rest.chars().next() {
        Some(c @ ('+' | '-')) if !numbered || rest[1..].starts_with(' ') => {
            (!l.starts_with("+++") && !l.starts_with("---")).then_some(c)
        }
        _ => None,
    }
}

fn lay_tool(tool: &Tool, width: usize, t: &Theme) -> Vec<Line<'static>> {
    let (dot, st) = match (tool.done, tool.is_error) {
        (false, _) => ("◌", t.warn),
        (true, false) => ("●", t.ok),
        (true, true) => ("●", t.error),
    };
    let mut head = vec![
        Span::styled(format!("{dot} "), st),
        Span::styled(tool.name.clone(), t.accent),
        Span::raw("  "),
        Span::styled(sanitize(&summary(&tool.name, &tool.args)), t.text),
    ];
    if let Some(ms) = tool.duration_ms.filter(|ms| *ms >= 1000) {
        head.push(Span::styled(format!("  {:.1}s", ms as f64 / 1000.0), t.dim));
    }
    let mut lines = vec![Line::default(), wrap::truncate(head, width)];
    if let Some(intent) = tool.args.get("intent").and_then(Value::as_str) {
        lines.push(wrap::truncate(
            vec![Span::styled(format!("  {}", sanitize(intent)), t.dim)],
            width,
        ));
    }
    let out = sanitize(&tool.output);
    let out = out.trim_end();
    if out.is_empty() {
        return lines;
    }
    let all: Vec<&str> = out.lines().collect();
    // Bash shows the tail (where errors are); edits show the head of the diff.
    let (shown, skipped_before, skipped_after) = match tool.name.as_str() {
        "edit" | "write" | "apply_patch" => {
            let n = all.len().min(OUTPUT_TAIL * 2);
            (&all[..n], 0, all.len() - n)
        }
        "read" if !tool.is_error => {
            lines.push(Line::styled(
                format!("  {} line{}", all.len(), if all.len() == 1 { "" } else { "s" }),
                t.dim,
            ));
            return lines;
        }
        _ => {
            let n = all.len().min(OUTPUT_TAIL);
            (&all[all.len() - n..], all.len() - n, 0)
        }
    };
    let more = |n: usize| Line::styled(format!("  … {n} more line{}", if n == 1 { "" } else { "s" }), t.dim);
    if skipped_before > 0 {
        lines.push(more(skipped_before));
    }
    for l in shown {
        let st = if tool.is_error {
            t.error
        } else {
            match diff_sign(l) {
                Some('+') => t.add,
                Some(_) => t.del,
                None => t.dim,
            }
        };
        lines.push(wrap::truncate(vec![Span::styled(format!("  {l}"), st)], width));
    }
    if skipped_after > 0 {
        lines.push(more(skipped_after));
    }
    lines
}
