//! The conversation as turns of items, laid out into visual lines (L9.1, L9.7,
//! L9.8). A finished turn is laid out once per width and cached; only the
//! turn still running is laid out again. Turn-collapse is built in.

use crate::markdown;
use crate::theme::Theme;
use crate::wrap::{self, sanitize};
use novacode_types::{Block, BlockKind, Message, StreamEvent};
use ratatui::text::{Line, Span};
use serde_json::Value;
use std::time::Instant;

const OUTPUT_TAIL: usize = 4;
const LIVE_CAP: usize = 64 * 1024;

pub enum Item {
    User(String),
    /// An image in a prompt or tool result (L9.14).
    Image(Img),
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
    /// When it started running; a running row shows a spinner and the seconds so far.
    pub started: Option<Instant>,
    /// `details.ui` of the result: styled lines the tool asks to show instead of its output.
    pub ui: Option<Vec<novacode_types::ui::Line>>,
}

#[derive(Default)]
pub struct Turn {
    pub items: Vec<Item>,
    pub done: bool,
    pub expanded: bool,
    cache: Option<Layout>,
}

/// An image, base64, with its pixel size (0 when unknown).
#[derive(Debug, Clone)]
pub struct Img {
    pub key: u64,
    pub data: std::sync::Arc<String>,
    pub w: u32,
    pub h: u32,
    /// Shown at size; folded to one line by default (click toggles).
    pub open: bool,
}

impl Img {
    fn new(data: &str) -> Self {
        let key = data
            .bytes()
            .fold(0xcbf29ce484222325u64, |h, b| (h ^ b as u64).wrapping_mul(0x100000001b3));
        let (w, h) = crate::images::dimensions(data).unwrap_or((0, 0));
        Img {
            key,
            data: std::sync::Arc::new(data.to_string()),
            w,
            h,
            open: false,
        }
    }

    /// Columns it takes at most.
    pub fn cols(&self, width: usize) -> usize {
        if self.w == 0 {
            width
        } else {
            (self.w as usize / 8).clamp(1, width.max(1))
        }
    }

    /// Rows at full size within `width` columns, at most `max` (cells are about twice as tall as wide).
    pub fn rows(&self, width: usize, max: usize) -> usize {
        if self.w == 0 || self.h == 0 {
            return max.min(8);
        }
        let cols = self.cols(width);
        (self.h as usize * cols / self.w as usize / 2).clamp(1, max.max(1))
    }
}

/// The most rows a tool's own `details.ui` lines take.
const TOOL_UI_ROWS: usize = 12;

struct Layout {
    width: usize,
    expanded: bool,
    lines: Vec<Line<'static>>,
    /// (first reserved line inside the turn, rows, image) for open images.
    images: Vec<(usize, usize, Img)>,
    /// (line inside the turn, item index) of image fold lines.
    image_toggles: Vec<(usize, usize)>,
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
    /// Ctrl+O (pi's tools.expand): every tool's output shown in full.
    pub tools_full: bool,
    /// Key that does that, named under cut tool output.
    pub tools_hint: String,
    /// Stream pacer (L9.7): bytes of each live block shown so far.
    pub(crate) live_shown: Vec<usize>,
    /// Tallest an open image may be, in rows (the transcript height); 0 = this
    /// terminal draws no images (L9.14).
    pub image_rows: usize,
    /// The prompt just submitted, shown until the agent's own copy arrives.
    pub echo: Option<String>,
    /// When the live message began streaming.
    pub(crate) live_since: Option<Instant>,
}

/// Braille spinner frames, one per 100 ms.
pub const SPINNER: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

fn spin_frame(since: Instant) -> &'static str {
    SPINNER[(since.elapsed().as_millis() / 100) as usize % SPINNER.len()]
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
    /// (first line, rows, image) to draw over the text.
    pub images: Vec<(usize, usize, Img)>,
    /// (line, turn, item) of image fold lines a click toggles.
    pub image_toggles: Vec<(usize, usize, usize)>,
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

    /// Rewrite the newest notice starting with `prefix` (a process's start line becomes
    /// its end line); a new notice when there is none.
    pub fn renotice(&mut self, prefix: &str, text: String, error: bool) {
        for turn in self.turns.iter_mut().rev() {
            for item in turn.items.iter_mut().rev() {
                if let Item::Notice { text: t, error: e } = item
                    && t.starts_with(prefix)
                {
                    (*t, *e) = (text, error);
                    turn.cache = None;
                    return;
                }
            }
        }
        self.notice(text, error);
    }

    /// Esc before the first reply: drop the last turn if it is only the prompt (and notices).
    pub fn take_back(&mut self) {
        if let Some(t) = self.turns.last()
            && matches!(t.items.first(), Some(Item::User(_)))
            && t.items
                .iter()
                .all(|i| matches!(i, Item::User(_) | Item::Image(_) | Item::Notice { .. }))
        {
            self.turns.pop();
        }
        self.live = None;
        self.live_shown.clear();
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

    /// Show or fold one image.
    pub fn toggle_image(&mut self, turn: usize, item: usize) {
        if let Some(t) = self.turns.get_mut(turn) {
            if let Some(Item::Image(img)) = t.items.get_mut(item) {
                img.open = !img.open;
            }
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
        if self.live.is_none() {
            self.live_since = Some(Instant::now());
        }
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
                self.echo = None;
                // Images follow as their own items (L9.14).
                let text = content
                    .iter()
                    .filter_map(|b| match b {
                        Block::Text { text, .. } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                // A saved reminder (`!cmd`, process exit, schedule, report) replayed from the
                // session: one quiet line, as it looked live, not the model-facing markup.
                if let Some(line) = reminder_line(&text) {
                    self.notice(line, false);
                    return;
                }
                for t in &mut self.turns {
                    if !t.done {
                        t.done = true;
                        t.expanded = self.expand_all;
                        t.touch();
                    }
                }
                let mut items = vec![Item::User(text)];
                items.extend(images_of(content));
                self.turns.push(Turn {
                    items,
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
                        Block::Text { text, replay } if !text.trim().is_empty() => {
                            Some((BlockKind::Text, with_sources(text, replay.as_ref())))
                        }
                        Block::Thinking { text, .. } if !text.trim().is_empty() => {
                            Some((BlockKind::Thinking, text.clone()))
                        }
                        Block::ProviderNative { item, .. } => searched(item).map(|q| (BlockKind::Text, q)),
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
                        started: None,
                        ui: None,
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
                details,
                ..
            } => {
                if let Some(t) = self.tool(call_id) {
                    // An edit keeps its diff out of the model's context; show it here.
                    t.output = details
                        .as_ref()
                        .and_then(|d| d["diff"].as_str())
                        .map_or_else(|| text_of(content), String::from);
                    t.is_error = *is_error;
                    t.done = true;
                    t.ui = details
                        .as_ref()
                        .and_then(|d| d.get("ui"))
                        .and_then(|v| serde_json::from_value(v.clone()).ok());
                }
                let imgs: Vec<Item> = images_of(content).collect();
                if !imgs.is_empty() {
                    let turn = self.turn();
                    turn.items.extend(imgs);
                    turn.touch();
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

    pub fn tool_start(&mut self, id: &str) {
        if let Some(t) = self.tool(id) {
            t.started = Some(Instant::now());
        }
    }

    /// Animation step: re-lay the newest turn while one of its tools runs.
    pub fn tick(&mut self) {
        if let Some(turn) = self.turns.last_mut()
            && turn
                .items
                .iter()
                .any(|i| matches!(i, Item::Tool(t) if !t.done && t.started.is_some()))
        {
            turn.touch();
        }
    }

    pub fn tool_end(&mut self, id: &str, is_error: bool, ms: u64) {
        if let Some(t) = self.tool(id) {
            t.duration_ms = Some(ms);
            t.is_error |= is_error;
        }
    }

    pub fn set_tools_full(&mut self, on: bool) {
        self.tools_full = on;
        for t in &mut self.turns {
            t.touch();
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
            if !matches!(kind, BlockKind::Text | BlockKind::Thinking) {
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
                    // Text and thinking are paced, so a burst climbs instead of jumping.
                    let n = match k {
                        BlockKind::Text | BlockKind::Thinking => {
                            self.live_shown.get(i).copied().unwrap_or(0).min(t.len())
                        }
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
        let since = Some(self.live_since.unwrap_or_else(Instant::now));
        let mut v = View {
            lines: Vec::new(),
            starts: Vec::new(),
            toggles: Vec::new(),
            images: Vec::new(),
            image_toggles: Vec::new(),
        };
        let images = self.image_rows;
        let last = self.turns.len().saturating_sub(1);
        for (i, turn) in self.turns.iter_mut().enumerate() {
            let fresh = turn
                .cache
                .as_ref()
                .is_some_and(|c| c.width == width && c.expanded == turn.expanded);
            if !fresh {
                let tools = (self.tools_full, self.tools_hint.as_str());
                turn.cache = Some(lay_turn(
                    turn,
                    width,
                    theme,
                    thinking,
                    &self.thinking_hint,
                    images,
                    tools,
                ));
            }
            let c = turn.cache.as_ref().unwrap();
            if i > 0 {
                v.lines.push(Line::default());
            }
            v.starts.push(v.lines.len());
            v.toggles.extend(c.toggles.iter().map(|o| (v.lines.len() + o, i)));
            let at = v.lines.len();
            v.images
                .extend(c.images.iter().map(|(o, r, img)| (at + o, *r, img.clone())));
            v.image_toggles
                .extend(c.image_toggles.iter().map(|(o, item)| (at + o, i, *item)));
            v.lines.extend(c.lines.iter().cloned());
            if i == last
                && let Some(live) = &live
            {
                v.lines
                    .extend(lay_assistant(live, width, theme, since, thinking, &hint));
            }
        }
        if self.turns.is_empty()
            && let Some(live) = &live
        {
            v.lines
                .extend(lay_assistant(live, width, theme, since, thinking, &hint));
        }
        if let Some(text) = &self.echo {
            let turn = Turn {
                items: vec![Item::User(text.clone())],
                ..Default::default()
            };
            if !v.lines.is_empty() {
                v.lines.push(Line::default());
            }
            let c = lay_turn(
                &turn,
                width,
                theme,
                thinking,
                &hint,
                images,
                (self.tools_full, &self.tools_hint),
            );
            v.lines.extend(c.lines);
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

fn images_of(content: &[Block]) -> impl Iterator<Item = Item> + '_ {
    content.iter().filter_map(|b| match b {
        Block::Image { data, .. } => Some(Item::Image(Img::new(data))),
        _ => None,
    })
}

fn lay_turn(
    turn: &Turn,
    width: usize,
    t: &Theme,
    thinking: Thinking,
    hint: &str,
    image_rows: usize,
    tools: (bool, &str),
) -> Layout {
    let mut lines = Vec::new();
    let mut toggles = Vec::new();
    let mut imgs = Vec::new();
    let mut image_toggles = Vec::new();
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
        // Images right after the prompt are part of it and never fold.
        let prompt_image = matches!(item, Item::Image(_))
            && turn.items[..idx]
                .iter()
                .all(|i| matches!(i, Item::User(_) | Item::Image(_)));
        let is_step = !matches!(item, Item::User(_) | Item::Notice { .. }) && Some(idx) != final_reply && !prompt_image;
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
                lines.extend(lay_assistant(&blocks, width, t, None, thinking, hint));
            }
            Item::Tool(tool) => lines.extend(lay_tool(tool, width, t, tools)),
            Item::Image(img) => {
                let size = if img.w > 0 {
                    format!(" {}×{}", img.w, img.h)
                } else {
                    String::new()
                };
                if image_rows == 0 {
                    // This terminal draws no images: a plain reference.
                    lines.push(Line::styled(format!("[image{size}]"), t.dim));
                } else {
                    // A fold line; when open, blank rows below it the image is drawn over.
                    image_toggles.push((lines.len(), idx));
                    let label = if img.open {
                        format!("▾ image{size}")
                    } else {
                        format!("▸ image{size} · click to show")
                    };
                    lines.push(Line::styled(label, t.toggle));
                    if img.open {
                        let rows = img.rows(width, image_rows);
                        imgs.push((lines.len(), rows, img.clone()));
                        lines.extend(std::iter::repeat_n(Line::default(), rows));
                    }
                }
            }
            Item::Notice { text, error } => {
                let st = if *error { t.error } else { t.dim };
                // Multi-line notices (`!cmd` output) keep their lines.
                for (i, l) in text.lines().enumerate() {
                    let first = if i > 0 {
                        "  "
                    } else if *error {
                        "! "
                    } else {
                        "· "
                    };
                    lines.extend(wrap::wrap(
                        vec![Span::styled(sanitize(l), st)],
                        width,
                        &[Span::styled(first, st)],
                        &[Span::raw("  ")],
                    ));
                }
            }
        }
    }
    Layout {
        width,
        expanded: turn.expanded,
        lines,
        toggles,
        images: imgs,
        image_toggles,
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
    live: Option<Instant>,
    thinking: Thinking,
    hint: &str,
) -> Vec<Line<'static>> {
    let mut lines = vec![Line::default()];
    let mut after_thinking = false;
    // Thinking that arrives as several blocks in a row (interleaved thinking) reads as one.
    let mut merged: Vec<(BlockKind, String)> = Vec::new();
    for (kind, text) in blocks {
        match merged.last_mut() {
            Some((BlockKind::Thinking, prev)) if *kind == BlockKind::Thinking => {
                prev.push_str("\n\n");
                prev.push_str(text.trim());
            }
            _ => merged.push((*kind, text.clone())),
        }
    }
    let n = merged.len();
    for (j, (kind, text)) in merged.iter().enumerate() {
        // One blank line between thinking and the reply text below it.
        if *kind == BlockKind::Text && std::mem::take(&mut after_thinking) {
            lines.push(Line::default());
        }
        after_thinking = *kind == BlockKind::Thinking && thinking != Thinking::Hidden;
        match kind {
            BlockKind::Thinking if thinking == Thinking::Hidden => {}
            BlockKind::Thinking if thinking == Thinking::Full => {
                // Line by line: `wrap::wrap` runs lines together, which glued paragraphs.
                for (i, l) in text.trim().lines().enumerate() {
                    if l.trim().is_empty() {
                        lines.push(Line::default());
                        continue;
                    }
                    let first = if i == 0 { "∴ " } else { "  " };
                    lines.extend(wrap::wrap(
                        vec![Span::styled(sanitize(l), t.thinking)],
                        width,
                        &[Span::styled(first, t.thinking)],
                        &[Span::raw("  ")],
                    ));
                }
            }
            // L9.7: by default thinking shows as one collapsed line.
            BlockKind::Thinking => {
                let words = text.split_whitespace().count();
                // Still thinking: dots cycle and the seconds tick (the provider sends thinking in bursts).
                let label = match live.filter(|_| j + 1 == n) {
                    Some(at) => {
                        let ms = at.elapsed().as_millis();
                        let dots = &"...   "[3 - (ms / 400 % 4) as usize..][..3];
                        format!("thinking{dots} {}s", ms / 1000)
                    }
                    None => "thought".into(),
                };
                let mut l = Line::styled(format!("∴ {label} · {words} words"), t.thinking);
                if !hint.is_empty() {
                    l.push_span(Span::styled(format!(" · {hint} to show"), t.dim));
                }
                lines.push(l);
            }
            BlockKind::Text => lines.extend(markdown::render(text, width, t, live.is_none())),
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
        "ask_user" => {
            let qs = args.get("questions").and_then(Value::as_array);
            let first = qs
                .and_then(|q| q.first())
                .and_then(|q| q.get("question"))
                .and_then(Value::as_str)
                .unwrap_or("");
            match qs.map_or(0, Vec::len) {
                0 | 1 => format!("\"{first}\""),
                n => format!("\"{first}\" (+{} more)", n - 1),
            }
        }
        "todo" => {
            let ids = |k: &str| {
                args.get(k)
                    .and_then(Value::as_array)
                    .filter(|v| !v.is_empty())
                    .map(|v| v.iter().map(|i| format!("#{i}")).collect::<Vec<_>>().join(" "))
            };
            let n = |k: &str| args.get(k).and_then(Value::as_array).map_or(0, Vec::len);
            let mut p = Vec::new();
            if n("set") > 0 {
                p.push(format!("plan of {}", n("set")));
            }
            if n("add") > 0 {
                p.push(format!("add {}", n("add")));
            }
            if let Some(d) = ids("done") {
                p.push(format!("done {d}"));
            }
            if let Some(d) = ids("drop") {
                p.push(format!("drop {d}"));
            }
            if let Some(i) = args.get("start").and_then(Value::as_u64) {
                p.push(format!("start #{i}"));
            }
            if p.is_empty() { "list".into() } else { p.join(" · ") }
        }
        "goal" => {
            let s = |k: &str| args.get(k).and_then(Value::as_str).unwrap_or("");
            match s("action") {
                "set" => format!("set \"{}\"", s("objective").lines().next().unwrap_or("")),
                "block" => format!("block: {}", s("reason").lines().next().unwrap_or("")),
                a => a.to_string(),
            }
        }
        "agent" if args.get("action").and_then(Value::as_str) == Some("spawn") => {
            let s = |k: &str| args.get(k).and_then(Value::as_str);
            format!(
                "spawn {} \"{}\"",
                s("agent").unwrap_or("worker"),
                s("title").or(s("task")).unwrap_or("").lines().next().unwrap_or("")
            )
        }
        "codemode" => match args.get("transaction").and_then(Value::as_bool) {
            Some(true) => "transaction".into(),
            _ => String::new(),
        },
        _ => key_values(args),
    }
}

/// pi's header form: `action="read" id="a10" ids=["a1","a2"]`, in argument order.
fn key_values(args: &Value) -> String {
    let Some(m) = args.as_object() else {
        return args.to_string();
    };
    m.iter()
        .filter(|(k, _)| k.as_str() != "intent")
        .map(|(k, v)| {
            let v = match v {
                Value::String(s) => format!("\"{}\"", s.lines().next().unwrap_or("")),
                v => v.to_string(),
            };
            format!("{k}={v}")
        })
        .collect::<Vec<_>>()
        .join(" ")
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

/// Rows of a tool's output shown before "… N more lines" (pi shows about this many).
const BODY_ROWS: usize = 10;
/// Cap for Ctrl+O's full view, so a huge output can't stall layout.
const FULL_ROWS: usize = 2000;

fn lay_tool(tool: &Tool, width: usize, t: &Theme, (full, hint): (bool, &str)) -> Vec<Line<'static>> {
    let (dot, st) = match (tool.done, tool.is_error) {
        (false, _) => (tool.started.map_or("◌", spin_frame), t.warn),
        (true, false) => ("●", t.ok),
        (true, true) => ("●", t.error),
    };
    let mut head = vec![
        Span::styled(format!("{dot} "), st),
        Span::styled(tool.name.clone(), t.accent.add_modifier(ratatui::style::Modifier::BOLD)),
        Span::raw(" "),
        Span::styled(sanitize(&summary(&tool.name, &tool.args)), t.text),
    ];
    if let Some(ms) = tool.duration_ms.filter(|ms| *ms >= 1000) {
        head.push(Span::styled(format!("  {:.1}s", ms as f64 / 1000.0), t.dim));
    } else if let Some(s) = tool
        .started
        .filter(|_| !tool.done)
        .map(|at| at.elapsed().as_secs())
        .filter(|s| *s >= 1)
    {
        head.push(Span::styled(format!("  {s}s"), t.dim));
    }
    let mut lines = vec![Line::default(), wrap::truncate(head, width)];
    if let Some(intent) = tool.args.get("intent").and_then(Value::as_str) {
        lines.push(wrap::truncate(
            vec![Span::styled(format!("  {}", sanitize(intent)), t.dim)],
            width,
        ));
    }
    let plural = |n: usize| if n == 1 { "line" } else { "lines" };
    let bar = [Span::styled("▏ ", t.border)];
    let more = |n: usize, what: &str| {
        let key = if hint.is_empty() {
            String::new()
        } else {
            format!(", {hint} to expand")
        };
        Line::styled(format!("▏ … ({n} more {what}{key})"), t.dim)
    };
    // codemode: the script itself, then what it printed.
    if tool.name == "codemode"
        && let Some(code) = tool.args.get("code").and_then(Value::as_str)
    {
        let code: Vec<&str> = code.trim_end().lines().collect();
        let n = if full {
            code.len().min(FULL_ROWS)
        } else {
            code.len().min(6)
        };
        for l in &code[..n] {
            lines.push(wrap::truncate(
                vec![bar[0].clone(), Span::styled(sanitize(l), t.code)],
                width,
            ));
        }
        if n < code.len() {
            lines.push(more(code.len() - n, "lines of code"));
        }
    }
    if let Some(ui) = &tool.ui {
        lines.extend(ui.iter().take(TOOL_UI_ROWS).map(|l| {
            let mut s = vec![Span::raw("  ")];
            s.extend(
                crate::panel::spans(l, t)
                    .into_iter()
                    .map(|sp| Span::styled(sanitize(&sp.content), sp.style)),
            );
            wrap::truncate(s, width)
        }));
        return lines;
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
            let n = if full {
                all.len().min(FULL_ROWS)
            } else {
                all.len().min(OUTPUT_TAIL * 2)
            };
            (&all[..n], 0, all.len() - n)
        }
        "read" if !tool.is_error && !full => {
            lines.push(Line::styled(
                format!("  {} line{}", all.len(), if all.len() == 1 { "" } else { "s" }),
                t.dim,
            ));
            return lines;
        }
        _ if full => {
            let n = all.len().min(FULL_ROWS);
            (&all[..n], 0, all.len() - n)
        }
        // Commands: the tail, where errors are.
        "bash" | "process" => {
            let n = all.len().min(OUTPUT_TAIL);
            (&all[all.len() - n..], all.len() - n, 0)
        }
        // Everything else (agent, ask_user, codemode, MCP, …): the start, wrapped, like pi.
        _ => {
            let st = if tool.is_error { t.error } else { t.text };
            let mut rows = Vec::new();
            let mut used = 0;
            for l in &all {
                let w = wrap::wrap(vec![Span::styled(l.to_string(), st)], width, &bar, &bar);
                if rows.len() + w.len() > BODY_ROWS && !rows.is_empty() {
                    break;
                }
                rows.extend(w);
                used += 1;
            }
            lines.extend(rows);
            if used < all.len() {
                lines.push(more(all.len() - used, plural(all.len() - used)));
            }
            return lines;
        }
    };
    if skipped_before > 0 {
        lines.push(more(skipped_before, plural(skipped_before)));
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
        lines.push(wrap::truncate(
            vec![bar[0].clone(), Span::styled(l.to_string(), st)],
            width,
        ));
    }
    if skipped_after > 0 {
        lines.push(more(skipped_after, plural(skipped_after)));
    }
    lines
}

/// A provider-run web search, as one line (L2.23).
fn searched(item: &serde_json::Value) -> Option<String> {
    let q = match item["type"].as_str()? {
        "server_tool_use" if item["name"] == "web_search" => item["input"]["query"].as_str(),
        "web_search_call" => item["action"]["query"].as_str(),
        _ => return None,
    };
    Some(format!("*searched the web: {}*", q.unwrap_or("…")))
}

/// Text plus the web sources it cites (Anthropic `citations`, OpenAI `url_citation`s).
fn with_sources(text: &str, replay: Option<&novacode_types::Replay>) -> String {
    let Some(c) = replay.and_then(|r| r.data["citations"].as_array()) else {
        return text.to_string();
    };
    let mut seen: Vec<&str> = Vec::new();
    let mut out = text.trim_end().to_string();
    for c in c {
        let Some(url) = c["url"].as_str() else { continue };
        if seen.contains(&url) {
            continue;
        }
        if seen.is_empty() {
            out.push_str("\n\nSources:");
        }
        seen.push(url);
        let title = c["title"].as_str().filter(|t| !t.is_empty()).unwrap_or(url);
        out.push_str(&format!("\n- [{title}]({url})"));
    }
    out
}

/// The one-line form of a `<system-reminder>` user message, or `None` for a real prompt.
fn reminder_line(text: &str) -> Option<String> {
    let body = text.trim().strip_prefix("<system-reminder>")?;
    let body = body.trim().trim_end_matches("</system-reminder>").trim();
    if let Some(rest) = body.strip_prefix("The user ran `")
        && let Some((cmd, _)) = rest.split_once('`')
    {
        return Some(format!("$ {cmd}"));
    }
    let first = body.lines().next().unwrap_or_default();
    let mut line: String = first.chars().take(100).collect();
    if first.chars().count() > 100 || body.lines().count() > 1 {
        line.push('…');
    }
    Some(line)
}

#[cfg(test)]
mod reminder_tests {
    use super::reminder_line;

    #[test]
    fn a_process_end_rewrites_its_start_line() {
        let mut tr = super::Transcript::default();
        tr.notice("⚙ proc_6 web: running in the process manager (/ps)", false);
        tr.notice("⚙ proc_63 x: running", false);
        tr.renotice("⚙ proc_6 ", "⚙ proc_6 web: stopped".into(), false);
        let texts: Vec<_> = tr.turns[0]
            .items
            .iter()
            .map(|i| match i {
                super::Item::Notice { text, .. } => text.as_str(),
                _ => "",
            })
            .collect();
        assert_eq!(texts, ["⚙ proc_6 web: stopped", "⚙ proc_63 x: running"]);
    }

    #[test]
    fn reminders_replay_as_one_line() {
        assert_eq!(
            reminder_line("<system-reminder>\nThe user ran `echo SECRET42` in /tmp. Output:\n```\nSECRET42\n```\n</system-reminder>").as_deref(),
            Some("$ echo SECRET42")
        );
        assert_eq!(
            reminder_line("<system-reminder>\n[schedule s1 fired] ping\n</system-reminder>").as_deref(),
            Some("[schedule s1 fired] ping")
        );
        assert_eq!(reminder_line("fix the <system-reminder> bug"), None);
    }
}

#[cfg(test)]
mod search_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn native_search_shows_query_and_sources() {
        let item = json!({"type": "server_tool_use", "name": "web_search", "input": {"query": "rust stable"}});
        assert_eq!(searched(&item).as_deref(), Some("*searched the web: rust stable*"));
        assert_eq!(
            searched(&json!({"type": "web_search_call", "action": {"query": "q"}})).as_deref(),
            Some("*searched the web: q*")
        );
        let r = novacode_types::Replay {
            provider: "anthropic".into(),
            data: json!({"citations": [{"url": "https://a", "title": "A"}, {"url": "https://a", "title": "A"}, {"url": "https://b"}]}),
        };
        assert_eq!(
            with_sources("1.99.1", Some(&r)),
            "1.99.1\n\nSources:\n- [A](https://a)\n- [https://b](https://b)"
        );
    }
}
