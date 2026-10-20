//! `/ps` in the side panel (L5.11, L9.16): one process at a time, its live output
//! with the program's own ANSI colours, or keyword colours when it has none.

use crate::theme::Theme;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use serde::{Deserialize, Serialize};

/// A process as the host reports it for the panel.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ProcView {
    pub id: String,
    pub name: String,
    pub command: String,
    /// `running`, `done`, `failed` or `stopped` (panel status names).
    pub status: String,
    /// `running`, `exited 0`, `exited 2`, `killed`, `stopped`.
    pub state: String,
    pub started_ms: u64,
    /// Output tail, stdout then `[stderr]` and stderr.
    pub output: String,
}

/// Running ones first, then the newest.
pub fn order(v: &mut [ProcView]) {
    v.sort_by(|a, b| {
        (b.status == "running")
            .cmp(&(a.status == "running"))
            .then(b.started_ms.cmp(&a.started_ms))
    });
}

/// The 16 basic colours as palette indexes: the terminal's own theme draws them.
fn basic(n: u8) -> Color {
    Color::Indexed(n)
}

/// One output line as spans: SGR colours kept, other escapes dropped. `None` = it had no colour.
fn ansi(line: &str, base: Style) -> (Vec<Span<'static>>, bool) {
    // A progress bar redraws with \r: only the last state shows.
    let line = line.rsplit('\r').next().unwrap_or(line);
    let mut out = Vec::new();
    let (mut st, mut cur, mut coloured) = (base, String::new(), false);
    let mut it = line.chars().peekable();
    while let Some(c) = it.next() {
        if c != '\x1b' {
            if !c.is_control() || c == '\t' {
                cur.push(if c == '\t' { ' ' } else { c });
            }
            continue;
        }
        match it.next() {
            Some('[') => {
                let mut params = String::new();
                let mut fin = ' ';
                for d in it.by_ref() {
                    if ('@'..='~').contains(&d) {
                        fin = d;
                        break;
                    }
                    params.push(d);
                }
                if fin != 'm' {
                    continue;
                }
                if !cur.is_empty() {
                    out.push(Span::styled(std::mem::take(&mut cur), st));
                }
                coloured = true;
                let p: Vec<u16> = params.split(';').map(|x| x.parse().unwrap_or(0)).collect();
                let mut i = 0;
                while i < p.len() {
                    match p[i] {
                        0 => st = base,
                        1 => st = st.add_modifier(Modifier::BOLD),
                        2 => st = st.add_modifier(Modifier::DIM),
                        3 => st = st.add_modifier(Modifier::ITALIC),
                        4 => st = st.add_modifier(Modifier::UNDERLINED),
                        22 => st = st.remove_modifier(Modifier::BOLD | Modifier::DIM),
                        23 => st = st.remove_modifier(Modifier::ITALIC),
                        24 => st = st.remove_modifier(Modifier::UNDERLINED),
                        n @ 30..=37 => st = st.fg(basic(n as u8 - 30)),
                        n @ 90..=97 => st = st.fg(basic(n as u8 - 90 + 8)),
                        39 => st.fg = base.fg,
                        38 if p.get(i + 1) == Some(&5) => {
                            st = st.fg(Color::Indexed(p.get(i + 2).copied().unwrap_or(7) as u8));
                            i += 2;
                        }
                        38 if p.get(i + 1) == Some(&2) => {
                            let c = |k: usize| p.get(i + k).copied().unwrap_or(0) as u8;
                            st = st.fg(Color::Rgb(c(2), c(3), c(4)));
                            i += 4;
                        }
                        // Backgrounds would fight the panel; skipped with their arguments.
                        48 => i += if p.get(i + 1) == Some(&5) { 2 } else { 4 },
                        _ => {}
                    }
                    i += 1;
                }
            }
            // OSC (titles, hyperlinks): skip to BEL or ESC \.
            Some(']') => {
                while let Some(d) = it.next() {
                    if d == '\x07' || (d == '\x1b' && it.next_if_eq(&'\\').is_some()) {
                        break;
                    }
                }
            }
            _ => {}
        }
    }
    if !cur.is_empty() {
        out.push(Span::styled(cur, st));
    }
    (out, coloured)
}

/// Colour by meaning when the program sent none: errors, warnings, success words, URLs.
fn keywords(text: &str, t: &Theme) -> Vec<Span<'static>> {
    let low = text.to_lowercase();
    let has = |ws: &[&str]| ws.iter().any(|w| low.contains(w));
    let base = if has(&["error", "failed", "panic", "fatal", "exception", "traceback"]) {
        t.error
    } else if has(&["warn", "deprecat"]) {
        t.warn
    } else if has(&[
        "ready",
        "success",
        "listening",
        "compiled",
        "passed",
        "finished",
        "done",
    ]) {
        t.ok
    } else {
        t.text
    };
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(i) = rest.find("http://").or_else(|| rest.find("https://")) {
        let end = rest[i..].find(char::is_whitespace).map_or(rest.len(), |e| i + e);
        if i > 0 {
            out.push(Span::styled(rest[..i].to_string(), base));
        }
        out.push(Span::styled(rest[i..end].to_string(), t.link));
        rest = &rest[end..];
    }
    if !rest.is_empty() {
        out.push(Span::styled(rest.to_string(), base));
    }
    out
}

/// The output as wrapped, coloured lines.
pub fn output_lines(p: &ProcView, w: usize, t: &Theme) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    for raw in p.output.lines() {
        if raw == "[stderr]" {
            out.push(Line::styled("── stderr ──", t.dim));
            continue;
        }
        let (spans, coloured) = ansi(raw, t.text);
        let spans = if coloured {
            spans
        } else {
            let plain: String = spans.iter().map(|s| s.content.as_ref()).collect();
            keywords(&plain, t)
        };
        out.extend(crate::wrap::wrap(spans, w.max(1), &[], &[]));
    }
    out
}

/// Header rows: status line, command, rule.
pub fn header(p: &ProcView, i: usize, n: usize, w: usize, nerd: bool, t: &Theme) -> Vec<Line<'static>> {
    let (g, gs) = crate::panel::status_glyph(&p.status, nerd, t);
    let elapsed = crate::panel::now_ms().saturating_sub(p.started_ms);
    let right = format!("‹ {}/{n} ›", i + 1);
    let mut left = vec![
        Span::styled(format!(" {g} "), gs),
        Span::styled(p.name.clone(), t.text.add_modifier(Modifier::BOLD)),
        Span::styled(format!("  {}", p.state), gs),
    ];
    if p.status == "running" {
        left.push(Span::styled(format!(" · {}", crate::panel::clock(elapsed)), t.dim));
    }
    let l = crate::wrap::truncate(left, w.saturating_sub(right.chars().count() + 1).max(1));
    let pad = w.saturating_sub(crate::wrap::width(&l.spans) + right.chars().count());
    let mut sp = l.spans;
    sp.push(Span::raw(" ".repeat(pad)));
    sp.push(Span::styled(right, t.accent));
    vec![
        Line::from(sp),
        crate::wrap::truncate(
            vec![Span::styled(" $ ", t.accent), Span::styled(p.command.clone(), t.dim)],
            w,
        ),
        Line::styled("─".repeat(w), t.border),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ansi_keeps_colours_and_drops_other_escapes() {
        let (s, c) = ansi("\x1b[1;32mok\x1b[0m done\x1b[2K", Style::new());
        assert!(c);
        assert_eq!(s[0].content, "ok");
        assert_eq!(s[0].style.fg, Some(Color::Indexed(2)));
        assert!(s[0].style.add_modifier.contains(Modifier::BOLD));
        assert_eq!(s[1].content, " done");
        assert_eq!(s[1].style.fg, None);
        let (s, _) = ansi("10%\r55%\r100% \x1b]0;title\x07x", Style::new());
        assert_eq!(s.iter().map(|x| x.content.as_ref()).collect::<String>(), "100% x");
        let (s, c) = ansi("\x1b[38;2;1;2;3mrgb", Style::new());
        assert!(c && s[0].style.fg == Some(Color::Rgb(1, 2, 3)));
    }

    #[test]
    fn keywords_colour_by_meaning() {
        let t = Theme::default();
        assert_eq!(keywords("Error: boom", &t)[0].style, t.error);
        let s = keywords("ready on http://localhost:3000 now", &t);
        assert_eq!(s[1].content, "http://localhost:3000");
        assert_eq!(s[1].style, t.link);
        let mut v = vec![
            ProcView {
                id: "a".into(),
                name: "a".into(),
                command: String::new(),
                status: "done".into(),
                state: "exited 0".into(),
                started_ms: 5,
                output: String::new(),
            };
            3
        ];
        v[1].status = "running".into();
        v[1].id = "r".into();
        v[2].started_ms = 9;
        v[2].id = "new".into();
        order(&mut v);
        assert_eq!([&v[0].id, &v[1].id, &v[2].id], ["r", "new", "a"]);
    }
}
