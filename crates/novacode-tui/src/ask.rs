//! The `ask_user` form (L5.21): one question at a time, choices with a cursor,
//! multi-select, a free-text row, a note on a choice (`n`), and the focused choice's preview.

use crate::theme::Theme;
use crate::wrap;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use novacode_types::{Question, Reply};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use std::collections::BTreeSet;
use tokio::sync::oneshot;

/// Questions from the session; it waits on `reply` (`None` = dismissed).
pub struct AskReq {
    /// Same id in every window; the first answer wins (L8.13).
    pub id: u64,
    pub questions: Vec<Question>,
    pub reply: oneshot::Sender<Option<Vec<Reply>>>,
}

/// The most preview rows a form shows.
const PREVIEW_ROWS: usize = 24;

pub struct AskForm {
    pub req: AskReq,
    q: usize,
    /// Cursor; `options.len()` = the free-text row.
    sel: Vec<usize>,
    picked: Vec<BTreeSet<usize>>,
    typed: Vec<String>,
    typing: bool,
    /// A note sent with the picked choice(s), and the row it was written on.
    notes: Vec<(String, usize)>,
    noting: bool,
}

pub enum Done {
    Pending,
    Answered(Vec<Reply>),
    Dismissed,
}

impl AskForm {
    pub fn new(req: AskReq) -> Self {
        let n = req.questions.len();
        let sel = req
            .questions
            .iter()
            .map(|q| q.options.iter().position(|o| o.recommended).unwrap_or(0))
            .collect();
        Self {
            req,
            q: 0,
            sel,
            picked: vec![BTreeSet::new(); n],
            typed: vec![String::new(); n],
            typing: false,
            notes: vec![(String::new(), 0); n],
            noting: false,
        }
    }

    fn cur(&self) -> &Question {
        &self.req.questions[self.q]
    }

    fn rows(&self) -> usize {
        self.cur().options.len() + 1
    }

    fn replies(&self) -> Vec<Reply> {
        self.req
            .questions
            .iter()
            .enumerate()
            .map(|(i, q)| {
                let mut chosen: Vec<String> = self.picked[i].iter().map(|&k| q.options[k].label.clone()).collect();
                // Single choice: the cursor is the answer unless text was typed.
                if !q.multi && chosen.is_empty() && self.typed[i].trim().is_empty() && self.sel[i] < q.options.len() {
                    chosen.push(q.options[self.sel[i]].label.clone());
                }
                // Typed text replaces the choice; a note goes along with it.
                let text = [self.typed[i].trim(), self.notes[i].0.trim()]
                    .into_iter()
                    .find(|t| !t.is_empty())
                    .map(String::from);
                Reply {
                    question: q.question.clone(),
                    chosen,
                    text,
                }
            })
            .collect()
    }

    /// Next question, or the end.
    fn advance(&mut self) -> Done {
        self.typing = false;
        self.noting = false;
        if self.q + 1 < self.req.questions.len() {
            self.q += 1;
            Done::Pending
        } else {
            Done::Answered(self.replies())
        }
    }

    pub fn handle(&mut self, k: KeyEvent) -> Done {
        let q = self.q;
        if self.noting {
            match k.code {
                // Single choice: the noted choice is the answer. Multi: it's picked, keep picking.
                KeyCode::Enter if !self.cur().multi => return self.advance(),
                KeyCode::Enter => {
                    self.picked[q].insert(self.notes[q].1);
                    self.noting = false;
                }
                KeyCode::Esc => self.noting = false,
                KeyCode::Backspace => {
                    self.notes[q].0.pop();
                }
                KeyCode::Char(c) if !k.modifiers.contains(KeyModifiers::CONTROL) => self.notes[q].0.push(c),
                _ => {}
            }
            return Done::Pending;
        }
        if self.typing {
            match k.code {
                KeyCode::Enter => return self.advance(),
                KeyCode::Esc => self.typing = false,
                KeyCode::Backspace => {
                    self.typed[q].pop();
                }
                KeyCode::Char(c) if !k.modifiers.contains(KeyModifiers::CONTROL) => self.typed[q].push(c),
                _ => {}
            }
            return Done::Pending;
        }
        let rows = self.rows();
        match k.code {
            KeyCode::Up | KeyCode::Char('k') => self.sel[q] = (self.sel[q] + rows - 1) % rows,
            KeyCode::Down | KeyCode::Char('j') => self.sel[q] = (self.sel[q] + 1) % rows,
            KeyCode::Char(' ') if self.cur().multi && self.sel[q] < rows - 1 => {
                let s = self.sel[q];
                if !self.picked[q].remove(&s) {
                    self.picked[q].insert(s);
                }
            }
            KeyCode::Char(d @ '1'..='9') => {
                let i = d as usize - '1' as usize;
                if i < rows - 1 {
                    self.sel[q] = i;
                    if !self.cur().multi {
                        return self.advance();
                    }
                    if !self.picked[q].remove(&i) {
                        self.picked[q].insert(i);
                    }
                }
            }
            KeyCode::Enter | KeyCode::Tab if self.sel[q] == rows - 1 => self.typing = true,
            KeyCode::Char('n') if self.sel[q] < rows - 1 => {
                self.notes[q].1 = self.sel[q];
                self.noting = true;
            }
            KeyCode::Enter | KeyCode::Tab => {
                if self.cur().multi && self.picked[q].is_empty() {
                    self.picked[q].insert(self.sel[q]);
                }
                return self.advance();
            }
            KeyCode::Left | KeyCode::BackTab if self.q > 0 => self.q -= 1,
            KeyCode::Esc => return Done::Dismissed,
            KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => return Done::Dismissed,
            _ => {}
        }
        Done::Pending
    }

    /// `spaced`: a blank line between the parts, dropped when the screen is short.
    fn lines(&self, w: usize, t: &Theme, spaced: bool) -> Vec<Line<'static>> {
        let q = self.cur();
        let mut out = Vec::new();
        let gap = |out: &mut Vec<Line<'static>>| {
            if spaced {
                out.push(Line::default());
            }
        };
        // The question on its own, then each choice: label on one line, its description under it.
        out.extend(wrap::wrap(
            vec![Span::styled(q.question.clone(), t.text.add_modifier(Modifier::BOLD))],
            w,
            &[],
            &[],
        ));
        gap(&mut out);
        let sel = self.sel[self.q];
        for (i, o) in q.options.iter().enumerate() {
            let focused = i == sel && !self.typing && !self.noting;
            let mark = if q.multi {
                if self.picked[self.q].contains(&i) {
                    "[x] "
                } else {
                    "[ ] "
                }
            } else {
                ""
            };
            let mut spans = vec![
                Span::styled(if focused { " ▸ " } else { "   " }, t.accent),
                Span::styled(format!("{}. {mark}", i + 1), t.dim),
                Span::styled(o.label.clone(), if focused { t.accent } else { t.text }),
            ];
            if o.recommended {
                spans.push(Span::styled("  recommended", t.ok));
            }
            let indent = [Span::raw(" ".repeat(6 + mark.len()))];
            out.extend(wrap::wrap(spans, w, &[], &indent));
            if let Some(d) = o.description.as_deref().filter(|d| !d.trim().is_empty()) {
                out.extend(wrap::wrap(
                    vec![Span::styled(d.to_string(), t.dim)],
                    w,
                    &indent,
                    &indent,
                ));
            }
            let (note, at) = &self.notes[self.q];
            if *at == i && (self.noting || !note.is_empty()) {
                let text = if self.noting {
                    format!("{note}▏")
                } else {
                    note.clone()
                };
                out.extend(wrap::wrap(
                    vec![
                        Span::styled("note: ", t.dim),
                        Span::styled(text, if self.noting { t.accent } else { t.text }),
                    ],
                    w,
                    &indent,
                    &indent,
                ));
            }
            if o.description.is_some() {
                gap(&mut out);
            }
        }
        let typed = &self.typed[self.q];
        let on_text = sel == q.options.len();
        let text = if self.typing {
            format!("{typed}▏")
        } else if typed.is_empty() {
            "Type something…".into()
        } else {
            typed.clone()
        };
        out.push(wrap::truncate(
            vec![
                Span::styled(if on_text && !self.typing { " ▸ " } else { "   " }, t.accent),
                Span::styled("✎ ", t.dim),
                Span::styled(text, if self.typing || on_text { t.accent } else { t.dim }),
            ],
            w,
        ));
        gap(&mut out);
        if let Some(p) = q.options.get(sel).and_then(|o| o.preview.as_ref()) {
            // Wrapped, not cut: a plan's lines are long. Capped so the keys line stays on screen.
            let bar = [Span::styled("   │ ", t.code)];
            let rows: Vec<Line<'static>> = p
                .lines()
                .flat_map(|l| wrap::wrap(vec![Span::styled(wrap::sanitize(l), t.code)], w, &bar, &bar))
                .collect();
            let more = rows.len().saturating_sub(PREVIEW_ROWS);
            out.extend(rows.into_iter().take(PREVIEW_ROWS));
            if more > 0 {
                out.push(Line::styled(format!("   │ … {more} more lines"), t.dim));
            }
            gap(&mut out);
        }
        let keys = if self.typing {
            "Enter answer · Esc back to the choices"
        } else if self.noting && q.multi {
            "Enter pick with note · Esc back"
        } else if self.noting {
            "Enter choose with note · Esc back"
        } else if q.multi {
            "↑↓ move · Space pick · n note · Enter next · ← back · Esc dismiss"
        } else {
            "↑↓ move · Enter choose · n note · 1-9 pick · ← back · Esc dismiss"
        };
        // Wrapped, so a narrow form never hides "Esc dismiss".
        out.extend(wrap::wrap(vec![Span::styled(keys, t.dim)], w, &[], &[]));
        out
    }

    pub fn height(&self, width: u16) -> u16 {
        self.lines(width.saturating_sub(4) as usize, &Theme::default(), true)
            .len() as u16
            + 2
    }

    pub fn render(&self, f: &mut Frame, area: Rect, t: &Theme) {
        let n = self.req.questions.len();
        let mut title = "Question".to_string();
        if n > 1 {
            title.push_str(&format!(" {} of {n}", self.q + 1));
        }
        if let Some(h) = &self.cur().header {
            title.push_str(&format!(" · {h}"));
        }
        let inner = crate::dialog::frame(f, area, &title, t);
        let w = inner.width as usize;
        let mut lines = self.lines(w, t, true);
        if lines.len() > inner.height as usize {
            lines = self.lines(w, t, false);
        }
        f.render_widget(Paragraph::new(lines), inner);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use novacode_types::Choice;

    fn key(c: KeyCode) -> KeyEvent {
        KeyEvent::new(c, KeyModifiers::NONE)
    }

    #[test]
    fn long_preview_lines_wrap() {
        let (tx, _rx) = oneshot::channel();
        let long = "word ".repeat(30);
        let f = AskForm::new(AskReq {
            id: 1,
            questions: vec![Question {
                question: "ok?".into(),
                header: None,
                options: vec![Choice {
                    label: "yes".into(),
                    description: None,
                    preview: Some(long.clone()),
                    recommended: false,
                }],
                multi: false,
            }],
            reply: tx,
        });
        let text: String = f
            .lines(40, &Theme::default(), true)
            .iter()
            .map(|l| crate::wrap::plain(l) + "\n")
            .collect();
        assert_eq!(text.matches("word").count(), 30, "nothing cut: {text}");
        assert!(
            text.lines().filter(|l| l.contains('│')).all(|l| !l.contains('…')),
            "{text}"
        );
    }

    /// Question alone at the top, each choice's description on its own lines under it,
    /// header and progress in the title; a short screen drops the blank lines, not the keys.
    #[test]
    fn layout_separates_question_and_choices() {
        let ch = |l: &str, d: &str| Choice {
            label: l.into(),
            description: Some(d.into()),
            preview: None,
            recommended: l.starts_with("Run both"),
        };
        let (tx, _rx) = oneshot::channel();
        let q = |n: &str| Question {
            question: format!("{n}: the model replies with two tool calls and no text. What does the loop do next?"),
            header: Some("Agent loop".into()),
            options: vec![
                ch(
                    "End the turn",
                    "Show the reply and wait for the user before running anything.",
                ),
                ch(
                    "Run both tools, then call the model again",
                    "Run them (in parallel, they are independent), append the results, send it back.",
                ),
            ],
            multi: false,
        };
        let f = AskForm::new(AskReq {
            id: 1,
            questions: vec![q("Q1"), q("Q2")],
            reply: tx,
        });
        let draw = |h: u16| {
            let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(60, h)).unwrap();
            term.draw(|fr| f.render(fr, fr.area(), &Theme::default())).unwrap();
            let b = term.backend().buffer().clone();
            (0..h)
                .map(|y| {
                    (0..60)
                        .map(|x| b[(x, y)].symbol().to_string())
                        .collect::<String>()
                        .trim_end()
                        .to_string()
                })
                .collect::<Vec<_>>()
        };
        let tall = draw(f.height(60));
        eprintln!("{}", tall.join("\n"));
        assert!(tall[0].contains("Question 1 of 2 · Agent loop"), "{tall:?}");
        let at = |s: &str| tall.iter().position(|l| l.contains(s)).unwrap();
        assert!(at("1. End the turn") > at("Q1:") + 1, "a blank line after the question");
        assert_eq!(
            at("Show the reply"),
            at("1. End the turn") + 1,
            "description under its label"
        );
        let r = at("Run both tools");
        assert!(tall[r..=r + 1].iter().any(|l| l.contains("recommended")));
        let short = draw(f.height(60) - 4);
        assert!(short.iter().any(|l| l.contains("Esc dismiss")), "{short:?}");
    }

    #[test]
    fn single_multi_and_typed() {
        let ch = |l: &str, rec: bool| Choice {
            label: l.into(),
            description: None,
            preview: None,
            recommended: rec,
        };
        let (tx, _rx) = oneshot::channel();
        let mut f = AskForm::new(AskReq {
            id: 1,
            questions: vec![
                Question {
                    question: "db?".into(),
                    header: None,
                    options: vec![ch("pg", false), ch("sqlite", true)],
                    multi: false,
                },
                Question {
                    question: "features?".into(),
                    header: None,
                    options: vec![ch("a", false), ch("b", false), ch("c", false)],
                    multi: true,
                },
                Question {
                    question: "name?".into(),
                    header: None,
                    options: vec![ch("x", false)],
                    multi: false,
                },
            ],
            reply: tx,
        });
        // The recommended choice starts focused.
        assert!(matches!(f.handle(key(KeyCode::Enter)), Done::Pending));
        f.handle(key(KeyCode::Char(' ')));
        f.handle(key(KeyCode::Down));
        f.handle(key(KeyCode::Down));
        f.handle(key(KeyCode::Char(' ')));
        f.handle(key(KeyCode::Enter));
        // Free text instead of a choice.
        f.handle(key(KeyCode::Down));
        f.handle(key(KeyCode::Enter));
        for c in "nova".chars() {
            f.handle(key(KeyCode::Char(c)));
        }
        let Done::Answered(r) = f.handle(key(KeyCode::Enter)) else {
            panic!("not done")
        };
        assert_eq!(r[0].chosen, ["sqlite"]);
        assert_eq!(r[1].chosen, ["a", "c"]);
        assert_eq!((r[2].chosen.len(), r[2].text.as_deref()), (0, Some("nova")));
        assert!(matches!(f.handle(key(KeyCode::Esc)), Done::Dismissed));
    }

    #[test]
    fn note_goes_with_the_choice() {
        let ch = |l: &str| Choice {
            label: l.into(),
            description: None,
            preview: None,
            recommended: false,
        };
        let (tx, _rx) = oneshot::channel();
        let mut f = AskForm::new(AskReq {
            id: 1,
            questions: vec![
                Question {
                    question: "db?".into(),
                    header: None,
                    options: vec![ch("pg"), ch("sqlite")],
                    multi: false,
                },
                Question {
                    question: "features?".into(),
                    header: None,
                    options: vec![ch("a"), ch("b")],
                    multi: true,
                },
            ],
            reply: tx,
        });
        f.handle(key(KeyCode::Char('n')));
        for c in "with pgvector".chars() {
            f.handle(key(KeyCode::Char(c)));
        }
        let shown: String = f
            .lines(60, &Theme::default(), false)
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
            .collect();
        assert!(shown.contains("note: with pgvector"));
        assert!(matches!(f.handle(key(KeyCode::Enter)), Done::Pending));
        // Multi: the note picks its row; Enter then moves on.
        f.handle(key(KeyCode::Down));
        f.handle(key(KeyCode::Char('n')));
        f.handle(key(KeyCode::Char('x')));
        f.handle(key(KeyCode::Enter));
        let Done::Answered(r) = f.handle(key(KeyCode::Enter)) else {
            panic!("not done")
        };
        assert_eq!(
            (r[0].chosen.clone(), r[0].text.as_deref()),
            (vec!["pg".to_string()], Some("with pgvector"))
        );
        assert_eq!(
            (r[1].chosen.clone(), r[1].text.as_deref()),
            (vec!["b".to_string()], Some("x"))
        );
    }
}
