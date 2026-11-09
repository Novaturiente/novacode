//! Styled word wrap and terminal-safe text. The transcript is laid out into
//! visual lines here, so scrolling and selection work on exact rows.

use ratatui::style::Style;
use ratatui::text::{Line, Span};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

/// Drop escape sequences and control characters so tool output can't move the
/// cursor or change colours. `\r` keeps only the text after the last one
/// (progress bars), tabs become 4 spaces.
pub fn sanitize(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\x1b' => match chars.next() {
                Some('[') => {
                    // CSI: parameters then one final byte in @..~
                    for c in chars.by_ref() {
                        if ('@'..='~').contains(&c) {
                            break;
                        }
                    }
                }
                Some(']') => {
                    // OSC: until BEL or ESC \
                    while let Some(c) = chars.next() {
                        if c == '\x07' || (c == '\x1b' && chars.peek() == Some(&'\\')) {
                            chars.next_if_eq(&'\\');
                            break;
                        }
                    }
                }
                _ => {}
            },
            '\n' => out.push('\n'),
            '\t' => out.push_str("    "),
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    continue;
                }
                let start = out.rfind('\n').map_or(0, |i| i + 1);
                out.truncate(start);
            }
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out
}

pub fn width(spans: &[Span]) -> usize {
    spans.iter().map(|s| s.content.width()).sum()
}

/// Word-wrap `content` to `width` cells. The first visual line gets `first`
/// in front, the rest get `rest` (hanging indent for lists and quotes).
pub fn wrap(
    content: Vec<Span<'static>>,
    width: usize,
    first: &[Span<'static>],
    rest: &[Span<'static>],
) -> Vec<Line<'static>> {
    let mut cells: Vec<(&str, Style)> = Vec::new();
    for s in &content {
        for g in s.content.graphemes(true) {
            cells.push((g, s.style));
        }
    }
    let mut out = Vec::new();
    let mut i = 0;
    let mut first_line = true;
    loop {
        let prefix = if first_line { first } else { rest };
        let avail = width.saturating_sub(self::width(prefix)).max(1);
        // Find how many graphemes fit, and the last break point after a space.
        let mut w = 0;
        let mut end = i;
        let mut brk = None;
        while end < cells.len() {
            let gw = cells[end].0.width();
            if w + gw > avail {
                break;
            }
            w += gw;
            if cells[end].0 == " " {
                brk = Some(end + 1);
            }
            end += 1;
        }
        let take = if end < cells.len() {
            brk.filter(|&b| b > i).unwrap_or(end.max(i + 1))
        } else {
            end
        };
        let mut line: Vec<Span<'static>> = prefix.to_vec();
        push_run(&mut line, &cells[i..take]);
        trim_end(&mut line);
        out.push(Line::from(line));
        i = take;
        first_line = false;
        // The next line doesn't start with the space we broke at.
        while i < cells.len() && cells[i].0 == " " {
            i += 1;
        }
        if i >= cells.len() {
            break;
        }
    }
    out
}

fn push_run(line: &mut Vec<Span<'static>>, cells: &[(&str, Style)]) {
    let mut cur = String::new();
    let mut style = None;
    for (g, s) in cells {
        if style.is_some_and(|st| st != *s) {
            line.push(Span::styled(std::mem::take(&mut cur), style.unwrap()));
        }
        style = Some(*s);
        cur.push_str(g);
    }
    if let Some(s) = style {
        line.push(Span::styled(cur, s));
    }
}

fn trim_end(line: &mut Vec<Span<'static>>) {
    while let Some(last) = line.last() {
        let t = last.content.trim_end().to_string();
        if t.is_empty() && line.len() > 1 {
            line.pop();
        } else {
            line.last_mut().unwrap().content = t.into();
            break;
        }
    }
}

/// Cut one line to `width` cells, adding `…` when cut.
pub fn truncate(spans: Vec<Span<'static>>, width: usize) -> Line<'static> {
    if self::width(&spans) <= width {
        return Line::from(spans);
    }
    let mut out = Vec::new();
    let mut w = 0;
    for s in spans {
        let mut cur = String::new();
        for g in s.content.graphemes(true) {
            if w + g.width() + 1 > width {
                out.push(Span::styled(cur, s.style));
                out.push(Span::styled("…", s.style));
                return Line::from(out);
            }
            w += g.width();
            cur.push_str(g);
        }
        out.push(Span::styled(cur, s.style));
    }
    Line::from(out)
}

/// Plain text of a line, for copy and tests.
pub fn plain(line: &Line) -> String {
    line.spans.iter().map(|s| s.content.as_ref()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(lines: &[Line]) -> Vec<String> {
        lines.iter().map(plain).collect()
    }

    #[test]
    fn wraps_at_words_with_hanging_indent() {
        let l = wrap(
            vec![Span::raw("the quick brown fox jumps")],
            12,
            &[Span::raw("• ")],
            &[Span::raw("  ")],
        );
        assert_eq!(texts(&l), ["• the quick", "  brown fox", "  jumps"]);
        // A word longer than the line is hard-broken.
        let l = wrap(vec![Span::raw("abcdefghij")], 4, &[], &[]);
        assert_eq!(texts(&l), ["abcd", "efgh", "ij"]);
        assert_eq!(texts(&wrap(vec![], 10, &[], &[])), [""]);
        // Wide characters count as two cells.
        assert_eq!(
            texts(&wrap(vec![Span::raw("日本語です")], 6, &[], &[])),
            ["日本語", "です"]
        );
    }

    #[test]
    fn sanitizes_terminal_codes() {
        assert_eq!(sanitize("\x1b[31mred\x1b[0m ok"), "red ok");
        assert_eq!(sanitize("a\x1b]0;title\x07b"), "ab");
        assert_eq!(sanitize("10%\r50%\r100%\nnext\r\n"), "100%\nnext\n");
        assert_eq!(sanitize("a\tb\x08"), "a    b");
    }

    #[test]
    fn truncates() {
        assert_eq!(plain(&truncate(vec![Span::raw("hello world")], 8)), "hello w…");
        assert_eq!(plain(&truncate(vec![Span::raw("hi")], 8)), "hi");
    }
}
