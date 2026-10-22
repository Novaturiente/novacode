//! `/btw` side chat: a centred popup with its own input. Each question rides on
//! the main conversation so far; nothing asked here is stored in the session.

use crate::dialog;
use crate::editor::{Action, Editor};
use crate::keys::A;
use crate::theme::Theme;
use crate::wrap;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::{Position, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

pub struct BtwChat {
    /// (question, answer); the answer is `None` while it is on its way.
    entries: Vec<(String, Option<Result<String, String>>)>,
    editor: Editor,
    /// Rows scrolled up from the bottom (0 = follow the newest).
    scroll: usize,
    /// Input width at the last draw, for the editor's line moves.
    width: usize,
}

pub enum BtwKey {
    None,
    Ask(String),
    Close,
}

impl Default for BtwChat {
    fn default() -> Self {
        Self {
            entries: Vec::new(),
            editor: Editor::default(),
            scroll: 0,
            width: 40,
        }
    }
}

impl BtwChat {
    pub fn ask(&mut self, q: String) {
        self.entries.push((q, None));
        self.scroll = 0;
    }

    /// Fill the open question. Gives the answer back when none is open.
    pub fn answer(&mut self, r: Result<String, String>) -> Result<(), Result<String, String>> {
        match self.entries.last_mut() {
            Some((_, a @ None)) => {
                *a = Some(r);
                self.scroll = 0;
                Ok(())
            }
            _ => Err(r),
        }
    }

    fn pending(&self) -> bool {
        matches!(self.entries.last(), Some((_, None)))
    }

    pub fn paste(&mut self, s: &str) {
        self.editor.paste(s);
    }

    /// `a` is the key's editor action from the keymap.
    pub fn handle(&mut self, k: KeyEvent, a: Option<A>, page: usize) -> BtwKey {
        match k.code {
            KeyCode::Esc => return BtwKey::Close,
            KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => return BtwKey::Close,
            KeyCode::PageUp => self.scroll += page,
            KeyCode::PageDown => self.scroll = self.scroll.saturating_sub(page),
            _ => match self.editor.handle(a, k, self.width) {
                Action::Submit(q) | Action::FollowUp(q) if self.pending() => {
                    // One question at a time, so the side thread keeps its order.
                    self.editor.set_text(&q);
                }
                Action::Submit(q) | Action::FollowUp(q) if !q.trim().is_empty() => return BtwKey::Ask(q),
                _ => {}
            },
        }
        BtwKey::None
    }

    pub fn render(&mut self, f: &mut Frame, area: Rect, t: &Theme) {
        let inner = dialog::frame(f, area, "btw: side chat · Enter ask · PgUp/PgDn scroll · Esc close", t);
        let w = inner.width as usize;
        self.width = w.saturating_sub(2).max(1);
        let (rows, (cr, cc)) = self.editor.view(self.width, 5);
        let input_h = rows.len().max(1) as u16;
        let body_h = inner.height.saturating_sub(input_h + 1);

        let mut body = Vec::new();
        if self.entries.is_empty() {
            body.push(Line::styled(
                "Ask a side question. It sees the conversation so far; nothing here is kept.",
                t.dim,
            ));
        }
        for (q, a) in &self.entries {
            let lead = [Span::styled("› ", t.accent)];
            body.extend(wrap::wrap(
                vec![Span::styled(q.clone(), t.accent)],
                w,
                &lead,
                &[Span::raw("  ")],
            ));
            match a {
                None => body.push(Line::styled("answering…", t.dim)),
                Some(Ok(a)) => body.extend(crate::markdown::render(a, w, t, true)),
                Some(Err(e)) => body.extend(wrap::wrap(vec![Span::styled(e.clone(), t.error)], w, &[], &[])),
            }
            body.push(Line::default());
        }
        let room = body_h as usize;
        self.scroll = self.scroll.min(body.len().saturating_sub(room));
        let end = body.len() - self.scroll;
        let shown = body[end.saturating_sub(room)..end].to_vec();
        f.render_widget(
            Paragraph::new(shown),
            Rect {
                height: body_h,
                ..inner
            },
        );

        let sep = inner.y + body_h;
        f.render_widget(
            Paragraph::new(Line::styled("─".repeat(w), t.dim)),
            Rect {
                y: sep,
                height: 1,
                ..inner
            },
        );
        let input: Vec<Line> = rows
            .into_iter()
            .enumerate()
            .map(|(i, r)| {
                let lead = if i == 0 { "› " } else { "  " };
                Line::from(vec![Span::styled(lead, t.accent), Span::styled(r, t.text)])
            })
            .collect();
        let iy = sep + 1;
        f.render_widget(
            Paragraph::new(input),
            Rect {
                y: iy,
                height: input_h,
                ..inner
            },
        );
        f.set_cursor_position(Position::new(inner.x + 2 + cc, iy + cr));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(c: &mut BtwChat, code: KeyCode) -> BtwKey {
        let k = KeyEvent::new(code, KeyModifiers::NONE);
        let a = crate::keys::Keymap::default().lookup(&k, &[crate::keys::Scope::Editor]);
        c.handle(k, a, 10)
    }

    #[test]
    fn asks_one_question_at_a_time() {
        let mut c = BtwChat::default();
        c.paste("why?");
        let BtwKey::Ask(q) = key(&mut c, KeyCode::Enter) else {
            panic!("no ask")
        };
        assert_eq!(q, "why?");
        c.ask(q);
        // A second question waits in the input until the first is answered.
        c.paste("and?");
        assert!(matches!(key(&mut c, KeyCode::Enter), BtwKey::None));
        assert_eq!(c.editor.text(), "and?");
        assert!(c.answer(Ok("because".into())).is_ok());
        assert!(c.answer(Ok("stray".into())).is_err(), "no open question");
        assert!(matches!(key(&mut c, KeyCode::Enter), BtwKey::Ask(q) if q == "and?"));
        assert!(matches!(key(&mut c, KeyCode::Esc), BtwKey::Close));
    }
}
