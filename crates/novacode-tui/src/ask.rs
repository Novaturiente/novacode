//! The `ask_user` form (L5.21): one question at a time, choices with a cursor,
//! multi-select, a free-text row, and the focused choice's preview.

use crate::theme::Theme;
use crate::wrap;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use novacode_types::{Question, Reply};
use ratatui::Frame;
use ratatui::layout::Rect;
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

pub struct AskForm {
    pub req: AskReq,
    q: usize,
    /// Cursor; `options.len()` = the free-text row.
    sel: Vec<usize>,
    picked: Vec<BTreeSet<usize>>,
    typed: Vec<String>,
    typing: bool,
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
                Reply {
                    question: q.question.clone(),
                    chosen,
                    text: Some(self.typed[i].trim().to_string()).filter(|t| !t.is_empty()),
                }
            })
            .collect()
    }

    /// Next question, or the end.
    fn advance(&mut self) -> Done {
        self.typing = false;
        if self.q + 1 < self.req.questions.len() {
            self.q += 1;
            Done::Pending
        } else {
            Done::Answered(self.replies())
        }
    }

    pub fn handle(&mut self, k: KeyEvent) -> Done {
        let q = self.q;
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

    fn lines(&self, w: usize, t: &Theme) -> Vec<Line<'static>> {
        let q = self.cur();
        let n = self.req.questions.len();
        let mut out = Vec::new();
        let mut head = Vec::new();
        if n > 1 {
            head.push(Span::styled(format!("{}/{n} ", self.q + 1), t.dim));
        }
        if let Some(h) = &q.header {
            head.push(Span::styled(format!("{h} · "), t.accent));
        }
        out.extend(wrap::wrap(
            vec![Span::styled(q.question.clone(), t.text)],
            w,
            &head,
            &[Span::raw("  ")],
        ));
        let sel = self.sel[self.q];
        for (i, o) in q.options.iter().enumerate() {
            let focused = i == sel && !self.typing;
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
                spans.push(Span::styled(" (recommended)", t.ok));
            }
            if let Some(d) = &o.description {
                spans.push(Span::styled(format!(" — {d}"), t.dim));
            }
            out.extend(wrap::wrap(spans, w, &[], &[Span::raw("      ")]));
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
        if let Some(p) = q.options.get(sel).and_then(|o| o.preview.as_ref()) {
            for l in p.lines().take(12) {
                out.push(wrap::truncate(
                    vec![Span::styled(format!("   │ {}", wrap::sanitize(l)), t.code)],
                    w,
                ));
            }
        }
        let keys = if self.typing {
            "Enter answer · Esc back to the choices"
        } else if q.multi {
            "↑↓ move · Space pick · Enter next · ← back · Esc dismiss"
        } else {
            "↑↓ move · Enter choose · 1-9 pick · ← back · Esc dismiss"
        };
        out.push(Line::styled(keys.to_string(), t.dim));
        out
    }

    pub fn height(&self, width: u16) -> u16 {
        self.lines(width.saturating_sub(2) as usize, &Theme::default()).len() as u16 + 2
    }

    pub fn render(&self, f: &mut Frame, area: Rect, t: &Theme) {
        let inner = crate::dialog::frame(f, area, "Question", t);
        f.render_widget(Paragraph::new(self.lines(inner.width as usize, t)), inner);
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
}
