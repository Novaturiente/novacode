//! Markdown to wrapped, styled lines (L9.6) with pulldown-cmark.

use crate::theme::Theme;
use crate::wrap::{self, sanitize};
use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

/// `highlight`: colour code blocks (off while streaming, so it never runs per frame).
pub fn render(md: &str, width: usize, t: &Theme, highlight: bool) -> Vec<Line<'static>> {
    let mut r = R {
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
    };
    let opts = Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS;
    for ev in Parser::new_ext(md, opts) {
        r.event(ev);
    }
    r.flush();
    while r
        .out
        .last()
        .is_some_and(|l| l.spans.iter().all(|s| s.content.trim().is_empty()))
    {
        r.out.pop();
    }
    r.out
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
}

impl R<'_> {
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
        // Unfinished markdown while streaming still renders.
        assert_eq!(md("```\npartial", 40), ["▏ partial"]);
    }
}
