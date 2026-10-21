//! Processes (L5.11, L9.16): the side panel lists them; `/ps` or Enter on one opens
//! this popup with its live output, in the program's own ANSI colours, or keyword
//! colours when it has none.

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

/// A line without its escape codes.
pub fn plain(line: &str) -> String {
    ansi(line, Style::new()).0.iter().map(|s| s.content.as_ref()).collect()
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

/// The `/ps` popup: one process (`id`, else the first) and how far its output is scrolled up.
pub struct ProcPopup {
    pub id: Option<String>,
    /// Rows scrolled up from the bottom (0 = follow the tail).
    scroll: usize,
}

impl ProcPopup {
    pub fn new(id: Option<String>) -> Self {
        Self { id, scroll: 0 }
    }

    fn index(&self, procs: &[ProcView]) -> usize {
        self.id
            .as_ref()
            .and_then(|id| procs.iter().position(|p| &p.id == id))
            .unwrap_or(0)
    }

    /// A key; true = close.
    pub fn handle(&mut self, k: &crossterm::event::KeyEvent, procs: &[ProcView], page: usize) -> bool {
        use crossterm::event::{KeyCode, KeyModifiers};
        let n = procs.len();
        let i = self.index(procs);
        let mut go = |j: usize| {
            self.id = procs.get(j).map(|p| p.id.clone());
            self.scroll = 0;
        };
        match k.code {
            KeyCode::Esc | KeyCode::Char('q') => return true,
            KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => return true,
            KeyCode::Left | KeyCode::Char('h') if n > 0 => go((i + n - 1) % n),
            KeyCode::Right | KeyCode::Char('l') if n > 0 => go((i + 1) % n),
            KeyCode::Up | KeyCode::Char('k') => self.scroll += 1,
            KeyCode::Down | KeyCode::Char('j') => self.scroll = self.scroll.saturating_sub(1),
            KeyCode::PageUp => self.scroll += page,
            KeyCode::PageDown => self.scroll = self.scroll.saturating_sub(page),
            KeyCode::Home | KeyCode::Char('g') => self.scroll = usize::MAX,
            KeyCode::End | KeyCode::Char('G') => self.scroll = 0,
            _ => {}
        }
        false
    }

    pub fn render(
        &mut self,
        f: &mut ratatui::Frame,
        area: ratatui::layout::Rect,
        procs: &[ProcView],
        nerd: bool,
        t: &Theme,
    ) {
        let inner = crate::dialog::frame(f, area, "Processes", t);
        let (w, h) = (inner.width as usize, inner.height as usize);
        let keys = Line::styled("←→ process  ↑↓ PgUp PgDn scroll  g/G top/tail  Esc close", t.dim);
        let mut out = Vec::new();
        if procs.is_empty() {
            out.push(Line::styled("no processes (long-running commands land here)", t.dim));
        } else {
            let i = self.index(procs);
            let p = &procs[i];
            out = header(p, i, procs.len(), w, nerd, t);
            let body = output_lines(p, w, t);
            let room = h.saturating_sub(out.len() + 1);
            self.scroll = self.scroll.min(body.len().saturating_sub(room));
            let end = body.len() - self.scroll;
            if body.is_empty() {
                out.push(Line::styled("(no output yet)", t.dim));
            }
            out.extend(body[end.saturating_sub(room)..end].iter().cloned());
        }
        out.resize(h.saturating_sub(1), Line::raw(""));
        out.push(keys);
        f.render_widget(ratatui::widgets::Paragraph::new(out), inner);
    }
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
    fn list_opens_the_popup_on_the_picked_process() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let pv = |id: &str, status: &str, out: &str| ProcView {
            id: id.into(),
            name: format!("{id}-name"),
            command: "npm run dev".into(),
            status: status.into(),
            state: status.into(),
            started_ms: 1,
            output: out.into(),
        };
        let mut panel = crate::panel::Panel::default();
        panel.set_procs(vec![
            pv("p1", "done", "built"),
            pv("p2", "running", "\x1b[32mready\x1b[0m on http://x"),
        ]);
        panel.shown = Some("processes".into());
        panel.focused = true;
        // Running first; the row shows the last output line without escapes.
        let shown = panel.current().unwrap().sources[0].nodes.clone();
        assert_eq!(shown[0].id, "p2");
        assert!(
            shown[0].detail.iter().any(|d| d == "› ready on http://x"),
            "{:?}",
            shown[0].detail
        );
        assert_eq!(
            panel.key("enter"),
            Some(Some(crate::panel::PanelAction::OpenProc("p2".into())))
        );
        // The popup: its output, ← → to the other process, Esc closes.
        let mut pop = ProcPopup::new(Some("p2".into()));
        let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(60, 12)).unwrap();
        let t = Theme::default();
        let draw = |pop: &mut ProcPopup, term: &mut ratatui::Terminal<ratatui::backend::TestBackend>| {
            term.draw(|f| pop.render(f, f.area(), &panel.procs, false, &t)).unwrap();
            term.backend()
                .buffer()
                .content()
                .iter()
                .map(|c| c.symbol())
                .collect::<String>()
        };
        assert!(draw(&mut pop, &mut term).contains("ready on http://x"));
        let k = |c| KeyEvent::new(c, KeyModifiers::NONE);
        assert!(!pop.handle(&k(KeyCode::Right), &panel.procs, 5));
        assert!(draw(&mut pop, &mut term).contains("built"));
        assert!(pop.handle(&k(KeyCode::Esc), &panel.procs, 5));
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
