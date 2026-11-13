//! Markdown to wrapped, styled lines (L9.6) with pulldown-cmark.

use crate::theme::Theme;
use crate::wrap::{self, sanitize};
use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

const OPTS: Options = Options::ENABLE_TABLES
    .union(Options::ENABLE_STRIKETHROUGH)
    .union(Options::ENABLE_TASKLISTS);

/// `highlight`: colour code blocks (off while streaming, so it never runs per frame).
pub fn render(md: &str, width: usize, t: &Theme, highlight: bool) -> Vec<Line<'static>> {
    render_widened(md, width, t, highlight, &[])
}

/// [`render`] with each table's columns at least `tables` wide (by table, then column):
/// a streamed reply keeps the widths its tables reached while streaming (TS3).
pub fn render_widened(md: &str, width: usize, t: &Theme, highlight: bool, tables: &[Vec<usize>]) -> Vec<Line<'static>> {
    let mut r = R::new(t, highlight, width);
    if !tables.is_empty() {
        r.min_table_widths = Some(tables.to_vec());
    }
    for ev in Parser::new_ext(md, OPTS) {
        r.event(ev);
    }
    r.finish()
}

/// A streaming reply's finished blocks, rendered once (TS3). A top-level block is
/// finished once the next one starts; only the text from there on is parsed again.
#[derive(Default)]
pub struct Stream {
    /// The source of the finished blocks.
    src: String,
    width: usize,
    lines: Vec<Line<'static>>,
    /// Column widths of each table still open; they only grow.
    tables: Vec<Vec<usize>>,
    /// Column widths of the tables in the finished blocks.
    done_tables: Vec<Vec<usize>>,
}

impl Stream {
    /// Every table's column widths so far, in order: for [`render_widened`].
    pub fn table_widths(&self) -> Vec<Vec<usize>> {
        self.done_tables.iter().chain(&self.tables).cloned().collect()
    }
}

/// Where the finished blocks of a streamed parse end.
struct Finished {
    /// Bytes of the parsed text.
    src: usize,
    /// Lines drawn.
    lines: usize,
    /// Tables seen.
    tables: usize,
}

/// [`render`] for text that only grows, without highlighting: reuses the finished
/// blocks' lines from `s`, and table columns never narrow.
pub fn render_live(md: &str, width: usize, t: &Theme, s: &mut Stream) -> Vec<Line<'static>> {
    if s.width != width || !md.starts_with(&s.src) {
        *s = Stream {
            width,
            ..Stream::default()
        };
    }
    let tail = &md[s.src.len()..];
    let mut r = R::new(t, false, width);
    r.out = s.lines.clone();
    r.min_table_widths = Some(std::mem::take(&mut s.tables));
    // Everything before the newest top-level block.
    let mut done = None;
    let mut depth = 0usize;
    let mut seen = false;
    for (ev, range) in Parser::new_ext(tail, OPTS).into_offset_iter() {
        if depth == 0 && matches!(ev, Event::Start(_) | Event::Rule) {
            // From the start of its line, so its indent parses the same.
            let at = tail[..range.start].rfind('\n').map_or(0, |i| i + 1);
            // A block on the unfinished last line may still turn into part of the one
            // above (`| 1` under a table), so that one isn't finished yet.
            if seen && tail[at..].contains('\n') {
                done = Some(Finished {
                    src: at,
                    lines: r.out.len(),
                    tables: r.tables_seen,
                });
            }
            seen = true;
        }
        match ev {
            Event::Start(_) => depth += 1,
            Event::End(_) => depth -= 1,
            _ => {}
        }
        r.event(ev);
    }
    let mut tables = r.min_table_widths.take().unwrap_or_default();
    if let Some(f) = done {
        s.src.push_str(&tail[..f.src]);
        s.lines = r.out[..f.lines].to_vec();
        s.done_tables.extend(tables.drain(..f.tables.min(tables.len())));
    }
    s.tables = tables;
    r.finish()
}

struct List {
    next: Option<u64>,
    indent: usize,
}

struct Table {
    rows: Vec<Vec<String>>,
    cell: String,
}

struct R<'a> {
    t: &'a Theme,
    highlight: bool,
    lang: String,
    width: usize,
    out: Vec<Line<'static>>,
    cur: Vec<Span<'static>>,
    styles: Vec<Style>,
    lists: Vec<List>,
    /// Bullet waiting for the first line of the current list item.
    bullet: Option<String>,
    quote: usize,
    code: Option<String>,
    links: Vec<String>,
    table: Option<Table>,
    /// Each table's column widths so far (streaming, or a streamed reply drawn again):
    /// no column is drawn narrower, and each grows to what this parse needs.
    min_table_widths: Option<Vec<Vec<usize>>>,
    tables_seen: usize,
    /// Inside an HTML comment, which isn't shown.
    comment: bool,
}

impl<'a> R<'a> {
    fn new(t: &'a Theme, highlight: bool, width: usize) -> Self {
        R {
            t,
            highlight,
            lang: String::new(),
            width,
            out: Vec::new(),
            cur: Vec::new(),
            styles: vec![t.text],
            lists: Vec::new(),
            bullet: None,
            quote: 0,
            code: None,
            links: Vec::new(),
            table: None,
            min_table_widths: None,
            tables_seen: 0,
            comment: false,
        }
    }

    /// The lines, without trailing blank ones.
    fn finish(mut self) -> Vec<Line<'static>> {
        self.flush();
        while self
            .out
            .last()
            .is_some_and(|l| l.spans.iter().all(|s| s.content.trim().is_empty()))
        {
            self.out.pop();
        }
        self.out
    }

    fn style(&self) -> Style {
        *self.styles.last().unwrap()
    }

    fn push_style(&mut self, f: impl Fn(Style) -> Style) {
        let s = f(self.style());
        self.styles.push(s);
    }

    fn prefixes(&mut self) -> (Vec<Span<'static>>, Vec<Span<'static>>) {
        let mut base = Vec::new();
        for _ in 0..self.quote {
            base.push(Span::styled("│ ", self.t.dim));
        }
        let depth = self.lists.len();
        let mut rest = base.clone();
        for (i, l) in self.lists.iter().enumerate() {
            if i + 1 < depth {
                base.push(Span::raw(" ".repeat(l.indent)));
            }
            rest.push(Span::raw(" ".repeat(l.indent)));
        }
        let mut first = base;
        match self.bullet.take() {
            Some(b) => first.push(Span::styled(b, self.t.accent)),
            None if depth > 0 => first.push(Span::raw(" ".repeat(self.lists[depth - 1].indent))),
            None => {}
        }
        (first, rest)
    }

    fn flush(&mut self) {
        if self.cur.is_empty() && self.bullet.is_none() {
            return;
        }
        let content = std::mem::take(&mut self.cur);
        let (first, rest) = self.prefixes();
        self.out.extend(wrap::wrap(content, self.width, &first, &rest));
    }

    fn gap(&mut self) {
        if self.lists.is_empty() && self.out.last().is_some_and(|l| wrap::width(&l.spans) > 0) {
            self.out.push(Line::default());
        }
    }

    fn text(&mut self, s: &str) {
        if let Some(t) = &mut self.table {
            t.cell.push_str(s);
        } else if let Some(c) = &mut self.code {
            c.push_str(s);
        } else {
            let st = self.style();
            self.cur.push(Span::styled(sanitize(s), st));
        }
    }

    fn event(&mut self, ev: Event) {
        let t = self.t;
        match ev {
            Event::Start(tag) => match tag {
                Tag::Paragraph => self.gap(),
                Tag::Heading { level, .. } => {
                    self.gap();
                    self.push_style(|_| t.heading);
                    if level <= HeadingLevel::H2 {
                        let s = self.style();
                        self.cur
                            .push(Span::styled(if level == HeadingLevel::H1 { "# " } else { "## " }, s));
                    }
                }
                Tag::BlockQuote(_) => {
                    self.flush();
                    self.gap();
                    self.quote += 1;
                    self.push_style(|_| t.quote);
                }
                Tag::CodeBlock(kind) => {
                    self.flush();
                    self.gap();
                    self.lang.clear();
                    if let CodeBlockKind::Fenced(lang) = kind
                        && !lang.is_empty()
                    {
                        self.lang = lang.to_string();
                        let (first, _) = self.prefixes();
                        let mut l = first;
                        l.push(Span::styled(format!("  {lang}"), t.dim));
                        self.out.push(Line::from(l));
                    }
                    self.code = Some(String::new());
                }
                Tag::List(start) => {
                    self.flush();
                    self.gap();
                    self.lists.push(List { next: start, indent: 2 });
                }
                Tag::Item => {
                    self.flush();
                    let l = self.lists.last_mut().unwrap();
                    let b = match &mut l.next {
                        Some(n) => {
                            let b = format!("{n}. ");
                            *n += 1;
                            b
                        }
                        None => "• ".into(),
                    };
                    l.indent = b.width();
                    self.bullet = Some(b);
                }
                Tag::Emphasis => self.push_style(|s| s.add_modifier(Modifier::ITALIC)),
                Tag::Strong => self.push_style(|s| s.add_modifier(Modifier::BOLD)),
                Tag::Strikethrough => self.push_style(|s| s.add_modifier(Modifier::CROSSED_OUT)),
                Tag::Link { dest_url, .. } => {
                    self.links.push(dest_url.to_string());
                    self.push_style(|_| t.link);
                }
                Tag::Table(_) => {
                    self.flush();
                    self.gap();
                    self.table = Some(Table {
                        rows: Vec::new(),
                        cell: String::new(),
                    });
                }
                Tag::TableHead | Tag::TableRow => {
                    if let Some(tb) = &mut self.table {
                        tb.rows.push(Vec::new());
                    }
                }
                _ => {}
            },
            Event::End(tag) => match tag {
                TagEnd::Paragraph => self.flush(),
                TagEnd::Heading(_) => {
                    self.flush();
                    self.styles.pop();
                }
                TagEnd::BlockQuote(_) => {
                    self.flush();
                    self.quote -= 1;
                    self.styles.pop();
                }
                TagEnd::CodeBlock => {
                    let code = sanitize(&self.code.take().unwrap_or_default());
                    let code = code.trim_end_matches('\n');
                    let (_, rest) = self.prefixes();
                    let mut bar = rest.clone();
                    bar.push(Span::styled("▏ ", t.border));
                    let colored = self.colored(code);
                    let plain = || {
                        code.split('\n')
                            .map(|l| vec![Span::styled(l.to_string(), t.code_block)])
                            .collect()
                    };
                    let lines: Vec<Vec<Span<'static>>> = colored.unwrap_or_else(plain);
                    for spans in lines {
                        self.out.extend(wrap::wrap(spans, self.width, &bar, &bar));
                    }
                }
                TagEnd::List(_) => {
                    self.flush();
                    self.lists.pop();
                }
                TagEnd::Item => self.flush(),
                TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough => {
                    self.styles.pop();
                }
                TagEnd::Link => {
                    self.styles.pop();
                    if let Some(url) = self.links.pop()
                        && !url.starts_with('#')
                    {
                        // In a table the URL belongs to the cell, not to the text after the table.
                        if let Some(tb) = &mut self.table {
                            if !tb.cell.ends_with(&url) {
                                tb.cell.push_str(&format!(" ({url})"));
                            }
                            return;
                        }
                        // Show the URL unless the text already is it.
                        let shown: String = self.cur.iter().rev().take(1).map(|s| s.content.to_string()).collect();
                        if shown != url {
                            self.cur.push(Span::styled(format!(" ({url})"), t.dim));
                        }
                    }
                }
                TagEnd::TableCell => {
                    if let Some(tb) = &mut self.table {
                        let c = std::mem::take(&mut tb.cell);
                        if let Some(row) = tb.rows.last_mut() {
                            row.push(sanitize(c.trim()));
                        }
                    }
                }
                TagEnd::Table => {
                    if let Some(tb) = self.table.take() {
                        self.table_lines(tb.rows);
                    }
                }
                _ => {}
            },
            Event::Text(s) => self.text(&s),
            Event::Code(s) => {
                if let Some(tb) = &mut self.table {
                    tb.cell.push_str(&s);
                } else {
                    self.cur.push(Span::styled(sanitize(&s), t.code));
                }
            }
            Event::SoftBreak => self.text(" "),
            Event::HardBreak => {
                self.flush();
            }
            Event::Rule => {
                self.flush();
                self.gap();
                self.out.push(Line::styled("─".repeat(self.width.min(40)), t.border));
            }
            Event::TaskListMarker(done) => {
                let st = if done { t.ok } else { t.dim };
                self.cur.push(Span::styled(if done { "[x] " } else { "[ ] " }, st));
            }
            // shortcut: text after a `-->` on the same line is hidden too; split on it if that shows up.
            Event::Html(s) | Event::InlineHtml(s) if self.comment || s.trim_start().starts_with("<!--") => {
                self.comment = !s.contains("-->");
            }
            Event::Html(s) | Event::InlineHtml(s) => self.text(&s),
            _ => {}
        }
    }

    #[cfg(feature = "highlight")]
    fn colored(&self, code: &str) -> Option<Vec<Vec<Span<'static>>>> {
        if !self.highlight || self.lang.is_empty() {
            return None;
        }
        crate::highlight::lines(code, &self.lang, &self.t.syntax, self.t.ansi256)
    }

    #[cfg(not(feature = "highlight"))]
    fn colored(&self, _code: &str) -> Option<Vec<Vec<Span<'static>>>> {
        let _ = (self.highlight, &self.lang); // built without the `highlight` feature
        None
    }

    fn table_lines(&mut self, rows: Vec<Vec<String>>) {
        let t = self.t;
        let cols = rows.iter().map(Vec::len).max().unwrap_or(0);
        let mut widths = vec![0; cols];
        for r in &rows {
            for (i, c) in r.iter().enumerate() {
                widths[i] = widths[i].max(c.width());
            }
        }
        if let Some(mins) = &mut self.min_table_widths {
            if mins.len() <= self.tables_seen {
                mins.resize(self.tables_seen + 1, Vec::new());
            }
            let seen = &mut mins[self.tables_seen];
            for (w, s) in widths.iter_mut().zip(seen.iter()) {
                *w = (*w).max(*s);
            }
            seen.clone_from(&widths);
        }
        self.tables_seen += 1;
        let gaps = cols.saturating_sub(1) * 3;
        let fits = widths.iter().sum::<usize>() + gaps <= self.width;
        if !fits && self.width >= gaps + cols * 3 {
            // Too wide: narrow columns get their full width, wide ones share the rest.
            let mut avail = self.width - gaps;
            let mut order: Vec<usize> = (0..cols).collect();
            order.sort_by_key(|&i| widths[i]);
            for (k, &i) in order.iter().enumerate() {
                widths[i] = widths[i].min(avail / (cols - k)).max(1);
                avail -= widths[i];
            }
        }
        let grid = fits || self.width >= gaps + cols * 3;
        for (n, r) in rows.iter().enumerate() {
            let style = if n == 0 {
                t.text.add_modifier(Modifier::BOLD)
            } else {
                t.text
            };
            if grid {
                // Wrap each cell inside its column; the row is as tall as its tallest cell.
                let cells: Vec<Vec<Line<'static>>> = (0..cols)
                    .map(|i| {
                        let c = r.get(i).map_or("", String::as_str);
                        wrap::wrap(vec![Span::styled(c.to_string(), style)], widths[i], &[], &[])
                    })
                    .collect();
                let height = cells.iter().map(Vec::len).max().unwrap_or(1);
                for h in 0..height {
                    let mut spans = Vec::new();
                    for (i, cell) in cells.iter().enumerate() {
                        if i > 0 {
                            spans.push(Span::styled(" │ ", t.border));
                        }
                        let part = cell.get(h).map(|l| l.spans.clone()).unwrap_or_default();
                        let pad = widths[i].saturating_sub(wrap::width(&part));
                        spans.extend(part);
                        if i + 1 < cols {
                            spans.push(Span::styled(" ".repeat(pad), style));
                        }
                    }
                    self.out.push(Line::from(spans));
                }
                if n == 0 {
                    let sep: Vec<String> = widths.iter().map(|w| "─".repeat(*w)).collect();
                    self.out.push(Line::styled(sep.join("─┼─"), t.border));
                }
            } else {
                // Too narrow even for a wrapped grid: one wrapped line per row.
                self.out.extend(wrap::wrap(
                    vec![Span::styled(r.join(" │ "), style)],
                    self.width,
                    &[],
                    &[],
                ));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn md(s: &str, w: usize) -> Vec<String> {
        render(s, w, &Theme::default(), true).iter().map(wrap::plain).collect()
    }

    #[test]
    fn html_comments_are_hidden() {
        let out = md("a <!-- note --> b\n\n<!--\nhidden\n-->\n\n<b>shown</b>\n", 40);
        assert_eq!(out, ["a  b", "", "<b>shown</b>"]);
    }

    #[test]
    fn blocks_lists_and_code() {
        let out = md(
            "# Title\n\nSome **bold** text that wraps.\n\n- one\n- two\n  - nested item here\n\n```rust\nfn main() {}\n```\n\n> quoted\n",
            20,
        );
        assert_eq!(
            out,
            [
                "# Title",
                "",
                "Some bold text that",
                "wraps.",
                "",
                "• one",
                "• two",
                "  • nested item here",
                "",
                "  rust",
                "▏ fn main() {}",
                "",
                "│ quoted",
            ]
        );
    }

    #[test]
    fn ordered_lists_tables_links() {
        assert_eq!(md("1. a\n2. b", 20), ["1. a", "2. b"]);
        assert_eq!(
            md("| a | bb |\n|---|---|\n| ccc | d |", 40),
            ["a   │ bb", "────┼───", "ccc │ d"]
        );
        // Too wide: cells wrap inside their columns, short columns keep full width.
        assert_eq!(
            md("| id | note |\n|---|---|\n| 1 | alpha beta gamma |", 14),
            ["id │ note", "───┼──────────", "1  │ alpha", "   │ beta", "   │ gamma"]
        );
        assert_eq!(md("[docs](https://x.y)", 40), ["docs (https://x.y)"]);
        assert_eq!(md("<https://x.y>", 40), ["https://x.y"]);
        // A link's URL stays in its table cell.
        assert_eq!(
            md("| [docs](https://x.io/d) | 2 |\n|---|---|\n\nNext para.", 60),
            [
                "docs (https://x.io/d) │ 2",
                "──────────────────────┼──",
                "",
                "Next para."
            ]
        );
        // Unfinished markdown while streaming still renders.
        assert_eq!(md("```\npartial", 40), ["▏ partial"]);
    }

    #[test]
    fn streamed_render_matches_a_full_parse() {
        let doc = "# Title\n\nSome *text* that wraps\nover lines.\n\npara\n- item\n  - nested\n\n1. one\n2. two\n\n> quote\nlazy\n\n---\n\n```rust\nfn x() {}\n```\n\nSetext\n---\n\n| a | b |\n|---|---|\n| 1 | 2 |\n\nend.";
        let t = Theme::default();
        let mut s = Stream::default();
        for (i, _) in doc.char_indices().skip(1).chain([(doc.len(), ' ')]) {
            let live: Vec<String> = render_live(&doc[..i], 24, &t, &mut s).iter().map(wrap::plain).collect();
            let full: Vec<String> = render(&doc[..i], 24, &t, false).iter().map(wrap::plain).collect();
            assert_eq!(live, full, "after {:?}", &doc[..i]);
        }
        assert!(!s.src.is_empty(), "finished blocks are kept");
    }
}
