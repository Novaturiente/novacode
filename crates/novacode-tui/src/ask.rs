//! The `ask_user` form (L5.21): one question at a time, choices with a cursor,
//! multi-select, a free-text row, a note on a choice (`n`), and the focused choice's preview.

use crate::editor::Editor;
use crate::keys::A;
use crate::theme::Theme;
use crate::wrap;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use novacode_types::{Question, Reply};
use ratatui::Frame;
use ratatui::layout::{Position, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use std::cell::Cell;
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
    typed: Vec<Editor>,
    typing: bool,
    /// A note sent with the picked choice(s), and the row it was written on.
    notes: Vec<(Editor, usize)>,
    noting: bool,
    /// Width of the open input at the last draw, for its up/down moves.
    width: Cell<usize>,
    /// (question, choice) whose preview the user rewrote in the editor.
    edited: BTreeSet<(usize, usize)>,
    /// The key that opens the editor, for the keys line (empty = not shown).
    pub edit_key: String,
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
            typed: (0..n).map(|_| Editor::default()).collect(),
            typing: false,
            notes: (0..n).map(|_| (Editor::default(), 0)).collect(),
            noting: false,
            width: Cell::new(40),
            edited: BTreeSet::new(),
            edit_key: String::new(),
        }
    }

    /// What the external editor opens: the text being typed, the note, or the focused choice's preview.
    pub fn edit_text(&self) -> Option<String> {
        let q = self.q;
        if self.typing {
            return Some(self.typed[q].full_text());
        }
        if self.noting {
            return Some(self.notes[q].0.full_text());
        }
        self.cur().options.get(self.sel[q])?.preview.clone()
    }

    /// The editor's result goes back where [`edit_text`](Self::edit_text) took it from.
    pub fn set_edited(&mut self, text: String) {
        let (q, k) = (self.q, self.sel[self.q]);
        if self.typing {
            self.typed[q].set_text(&text);
        } else if self.noting {
            self.notes[q].0.set_text(&text);
        } else if let Some(o) = self.req.questions[q].options.get_mut(k) {
            o.preview = Some(text);
            self.edited.insert((q, k));
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
                if !q.multi
                    && chosen.is_empty()
                    && self.typed[i].text().trim().is_empty()
                    && self.sel[i] < q.options.len()
                {
                    chosen.push(q.options[self.sel[i]].label.clone());
                }
                // Typed text replaces the choice; a note goes along with it.
                let (typed, note) = (self.typed[i].full_text(), self.notes[i].0.full_text());
                let text = [typed.trim(), note.trim()]
                    .into_iter()
                    .find(|t| !t.is_empty())
                    .map(String::from);
                // A preview the user rewrote (an edited plan): the chosen one's, else any.
                let edited = |k: &usize| self.edited.contains(&(i, *k));
                let preview = (0..q.options.len())
                    .filter(edited)
                    .max_by_key(|&k| chosen.contains(&q.options[k].label))
                    .and_then(|k| q.options[k].preview.clone());
                Reply {
                    question: q.question.clone(),
                    chosen,
                    text,
                    preview,
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

    /// A paste goes into the open input, if any.
    pub fn paste(&mut self, s: &str) {
        let q = self.q;
        if self.typing {
            self.typed[q].paste(s);
        } else if self.noting {
            self.notes[q].0.paste(s);
        }
    }

    /// `a` is the key's editor action from the keymap; the open input edits like the prompt.
    pub fn handle(&mut self, k: KeyEvent, a: Option<A>) -> Done {
        let q = self.q;
        if self.typing || self.noting {
            let ed = if self.typing {
                &mut self.typed[q]
            } else {
                &mut self.notes[q].0
            };
            // `\` then Enter stays a newline, as in the prompt.
            let submit = a == Some(A::Submit) && !ed.text()[..ed.cursor()].ends_with('\\');
            if k.code == KeyCode::Esc {
                self.typing = false;
                self.noting = false;
            } else if !submit {
                ed.handle(a, k, self.width.get());
            } else if self.typing || !self.cur().multi {
                // Single choice: the noted choice is the answer.
                return self.advance();
            } else {
                // Multi: the noted choice is picked; keep picking.
                self.picked[q].insert(self.notes[q].1);
                self.noting = false;
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
    /// Also gives the open input's cursor (row, column).
    fn lines(&self, w: usize, t: &Theme, spaced: bool) -> (Vec<Line<'static>>, Option<(usize, usize)>) {
        let q = self.cur();
        let mut out = Vec::new();
        let mut cursor = None;
        // An input's soft-wrapped rows: the first after `lead`, the rest lined up under it.
        let field =
            |out: &mut Vec<Line<'static>>, ed: &Editor, lead: Vec<Span<'static>>, style: Style, active: bool| {
                let pad: usize = lead.iter().map(|s| s.width()).sum();
                let ew = w.saturating_sub(pad).max(2);
                if active {
                    self.width.set(ew);
                }
                let (rows, (cr, cc)) = ed.view(ew, usize::MAX);
                let at = active.then(|| (out.len() + cr as usize, pad + cc as usize));
                for (i, r) in rows.into_iter().enumerate() {
                    let mut spans = if i == 0 {
                        lead.clone()
                    } else {
                        vec![Span::raw(" ".repeat(pad))]
                    };
                    spans.push(Span::styled(r, style));
                    out.push(Line::from(spans));
                }
                at
            };
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
                let lead = vec![indent[0].clone(), Span::styled("note: ", t.dim)];
                let style = if self.noting { t.accent } else { t.text };
                cursor = field(&mut out, note, lead, style, self.noting).or(cursor);
            }
            if o.description.is_some() {
                gap(&mut out);
            }
        }
        let typed = &self.typed[self.q];
        let on_text = sel == q.options.len();
        let lead = vec![
            Span::styled(if on_text && !self.typing { " ▸ " } else { "   " }, t.accent),
            Span::styled("✎ ", t.dim),
        ];
        let style = if self.typing || on_text { t.accent } else { t.dim };
        if self.typing || !typed.is_empty() {
            cursor = field(&mut out, typed, lead, style, self.typing).or(cursor);
        } else {
            out.push(Line::from(
                [lead, vec![Span::styled("Type something…", style)]].concat(),
            ));
        }
        gap(&mut out);
        if let Some(p) = q.options.get(sel).and_then(|o| o.preview.as_ref()) {
            // Wrapped, not cut: a plan's lines are long. Capped so the keys line stays on screen.
            if self.edited.contains(&(self.q, sel)) {
                out.push(Line::styled("   edited — the choice sends this text", t.accent));
            }
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
        let has_preview = q.options.get(sel).is_some_and(|o| o.preview.is_some());
        let mut keys = if self.typing {
            "Enter answer · Esc back to the choices"
        } else if self.noting && q.multi {
            "Enter pick with note · Esc back"
        } else if self.noting {
            "Enter choose with note · Esc back"
        } else if q.multi {
            "↑↓ move · Space pick · n note · Enter next · ← back · Esc dismiss"
        } else {
            "↑↓ move · Enter choose · n note · 1-9 pick · ← back · Esc dismiss"
        }
        .to_string();
        if !self.edit_key.is_empty() && (self.typing || self.noting || has_preview) {
            let what = if has_preview && !self.typing && !self.noting {
                "view/edit in editor"
            } else {
                "editor"
            };
            keys = format!("{} {what} · {keys}", self.edit_key);
        }
        // Wrapped, so a narrow form never hides "Esc dismiss".
        out.extend(wrap::wrap(vec![Span::styled(keys, t.dim)], w, &[], &[]));
        (out, cursor)
    }

    pub fn height(&self, width: u16) -> u16 {
        self.lines(width.saturating_sub(4) as usize, &Theme::default(), true)
            .0
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
        if lines.0.len() > inner.height as usize {
            lines = self.lines(w, t, false);
        }
        let (lines, cursor) = lines;
        f.render_widget(Paragraph::new(lines), inner);
        if let Some((r, c)) = cursor.filter(|&(r, _)| r < inner.height as usize) {
            f.set_cursor_position(Position::new(inner.x + c as u16, inner.y + r as u16));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use novacode_types::Choice;

    fn key(c: KeyCode) -> KeyEvent {
        KeyEvent::new(c, KeyModifiers::NONE)
    }

    fn hit(f: &mut AskForm, c: KeyCode) -> Done {
        let k = key(c);
        f.handle(
            k,
            crate::keys::Keymap::default().lookup(&k, &[crate::keys::Scope::Editor]),
        )
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
            .0
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
        assert!(matches!(hit(&mut f, KeyCode::Enter), Done::Pending));
        hit(&mut f, KeyCode::Char(' '));
        hit(&mut f, KeyCode::Down);
        hit(&mut f, KeyCode::Down);
        hit(&mut f, KeyCode::Char(' '));
        hit(&mut f, KeyCode::Enter);
        // Free text instead of a choice.
        hit(&mut f, KeyCode::Down);
        hit(&mut f, KeyCode::Enter);
        for c in "nova".chars() {
            hit(&mut f, KeyCode::Char(c));
        }
        let Done::Answered(r) = hit(&mut f, KeyCode::Enter) else {
            panic!("not done")
        };
        assert_eq!(r[0].chosen, ["sqlite"]);
        assert_eq!(r[1].chosen, ["a", "c"]);
        assert_eq!((r[2].chosen.len(), r[2].text.as_deref()), (0, Some("nova")));
        assert!(matches!(hit(&mut f, KeyCode::Esc), Done::Dismissed));
    }

    #[test]
    fn edited_plan_goes_with_the_answer() {
        let mut approve = Choice {
            label: "Approve".into(),
            description: None,
            preview: Some("1. old step".into()),
            recommended: false,
        };
        let keep = Choice {
            preview: None,
            label: "Keep planning".into(),
            ..approve.clone()
        };
        approve.recommended = true;
        let (tx, _rx) = oneshot::channel();
        let mut f = AskForm::new(AskReq {
            id: 1,
            questions: vec![Question {
                question: "Approve this plan?".into(),
                header: None,
                options: vec![approve, keep],
                multi: false,
            }],
            reply: tx,
        });
        f.edit_key = "ctrl+g".into();
        assert_eq!(f.edit_text().as_deref(), Some("1. old step"));
        let keys: String = f.lines(80, &Theme::default(), true).0.iter().map(wrap::plain).collect();
        assert!(keys.contains("ctrl+g view/edit in editor"), "{keys}");
        f.set_edited("1. new step".into());
        let Done::Answered(r) = hit(&mut f, KeyCode::Enter) else {
            panic!()
        };
        assert_eq!(
            (r[0].chosen[0].as_str(), r[0].preview.as_deref()),
            ("Approve", Some("1. new step"))
        );
    }

    /// The note input edits like the prompt (word delete, Shift+Backspace) and wraps long text.
    #[test]
    fn note_is_a_real_input() {
        let (tx, _rx) = oneshot::channel();
        let mut f = AskForm::new(AskReq {
            id: 1,
            questions: vec![Question {
                question: "db?".into(),
                header: None,
                options: vec![Choice {
                    label: "pg".into(),
                    description: None,
                    preview: None,
                    recommended: false,
                }],
                multi: false,
            }],
            reply: tx,
        });
        hit(&mut f, KeyCode::Char('n'));
        f.paste(&"word ".repeat(20));
        let k = KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL);
        f.handle(
            k,
            crate::keys::Keymap::default().lookup(&k, &[crate::keys::Scope::Editor]),
        );
        let k = KeyEvent::new(KeyCode::Backspace, KeyModifiers::SHIFT);
        f.handle(
            k,
            crate::keys::Keymap::default().lookup(&k, &[crate::keys::Scope::Editor]),
        );
        let (lines, cursor) = f.lines(30, &Theme::default(), false);
        let text: Vec<String> = lines.iter().map(wrap::plain).collect();
        assert!(text.iter().all(|l| l.chars().count() <= 30), "{text:?}");
        assert!(cursor.is_some());
        let Done::Answered(r) = hit(&mut f, KeyCode::Enter) else {
            panic!("not done")
        };
        assert_eq!(r[0].text.as_deref(), Some("word ".repeat(19).trim()));
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
        hit(&mut f, KeyCode::Char('n'));
        for c in "with pgvector".chars() {
            hit(&mut f, KeyCode::Char(c));
        }
        let shown: String = f
            .lines(60, &Theme::default(), false)
            .0
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
            .collect();
        assert!(shown.contains("note: with pgvector"));
        assert!(matches!(hit(&mut f, KeyCode::Enter), Done::Pending));
        // Multi: the note picks its row; Enter then moves on.
        hit(&mut f, KeyCode::Down);
        hit(&mut f, KeyCode::Char('n'));
        hit(&mut f, KeyCode::Char('x'));
        hit(&mut f, KeyCode::Enter);
        let Done::Answered(r) = hit(&mut f, KeyCode::Enter) else {
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
