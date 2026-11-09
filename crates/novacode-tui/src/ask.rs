//! The `ask_user` form (L5.21): one question at a time, choices with a cursor,
//! multi-select, free text as the last numbered choice, a note on a choice (`n`), and the
//! focused choice's preview. A click selects a choice, a double-click confirms it (TP5).

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
use std::cell::{Cell, RefCell};
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

#[derive(Default)]
struct Sections {
    /// First row of each choice, then of the free-text row, then where that row ends.
    choices: Vec<usize>,
    context_end: usize,
    preview_start: usize,
    help_start: usize,
}

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
    /// Where the form was last drawn, and which screen rows hold which choice.
    area: Cell<Rect>,
    choice_rows: RefCell<Vec<(u16, usize)>>,
    clicks: crate::dialog::Clicks,
    context_scroll: Cell<usize>,
    context_page: Cell<usize>,
    context_max: Cell<usize>,
    /// Number shortcuts may only confirm choices shown in the last draw.
    visible_shortcuts: Cell<u16>,
    rendered_selection: Cell<Option<(usize, usize)>>,
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
            area: Cell::new(Rect::default()),
            choice_rows: RefCell::new(Vec::new()),
            clicks: Default::default(),
            context_scroll: Cell::new(0),
            context_page: Cell::new(1),
            context_max: Cell::new(0),
            visible_shortcuts: Cell::new(u16::MAX),
            rendered_selection: Cell::new(None),
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
        self.context_scroll.set(0);
        if self.q + 1 < self.req.questions.len() {
            self.q += 1;
            Done::Pending
        } else {
            Done::Answered(self.replies())
        }
    }

    /// The form covers this screen cell.
    pub fn contains(&self, x: u16, y: u16) -> bool {
        self.area.get().contains(Position::new(x, y))
    }

    /// A press on screen row `y`: selects the choice there; a second press confirms it
    /// (free text opens for typing).
    pub fn click(&mut self, y: u16) -> Done {
        let hit = self.choice_rows.borrow().iter().find(|(r, _)| *r == y).map(|(_, c)| *c);
        let Some(i) = hit else { return Done::Pending };
        let double = self.clicks.double(y);
        let q = self.q;
        self.typing = false;
        self.noting = false;
        self.sel[q] = i;
        if !double {
            return Done::Pending;
        }
        if i == self.rows() - 1 {
            self.typing = true;
            return Done::Pending;
        }
        if self.cur().multi && self.picked[q].is_empty() {
            self.picked[q].insert(i);
        }
        self.advance()
    }

    /// A wheel step scrolls the context of a form that does not fit (`up` toward the top).
    pub fn wheel(&mut self, up: bool) {
        let (s, w) = (self.context_scroll.get(), crate::dialog::WHEEL);
        self.context_scroll.set(if up {
            s.saturating_sub(w)
        } else {
            (s + w).min(self.context_max.get())
        });
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
        if matches!(k.code, KeyCode::PageUp | KeyCode::PageDown) {
            let scroll = self.context_scroll.get();
            self.context_scroll.set(if k.code == KeyCode::PageUp {
                scroll.saturating_sub(self.context_page.get())
            } else {
                scroll
                    .saturating_add(self.context_page.get())
                    .min(self.context_max.get())
            });
            return Done::Pending;
        }
        // Navigation and confirmation can arrive before the next draw.
        if matches!(k.code, KeyCode::Enter | KeyCode::Tab)
            && self
                .rendered_selection
                .get()
                .is_some_and(|shown| shown != (q, self.sel[q]))
        {
            return Done::Pending;
        }
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
                    if self.visible_shortcuts.get() & (1 << i) == 0
                        || self.rendered_selection.get().is_some_and(|(shown_q, _)| shown_q != q)
                    {
                        return Done::Pending;
                    }
                    if !self.cur().multi {
                        return self.advance();
                    }
                    if !self.picked[q].remove(&i) {
                        self.picked[q].insert(i);
                    }
                } else if i == rows - 1 {
                    // Free text is the last numbered choice.
                    self.sel[q] = i;
                    self.typing = true;
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
            KeyCode::Left | KeyCode::BackTab if self.q > 0 => {
                self.q -= 1;
                self.context_scroll.set(0);
            }
            KeyCode::Esc => return Done::Dismissed,
            KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => return Done::Dismissed,
            _ => {}
        }
        Done::Pending
    }

    /// `spaced`: a blank line between the parts, dropped when the screen is short.
    /// Also gives the open input's cursor (row, column).
    fn lines(&self, w: usize, t: &Theme, spaced: bool) -> (Vec<Line<'static>>, Option<(usize, usize)>) {
        let (lines, cursor, _) = self.content(w, t, spaced, "enter");
        (lines, cursor)
    }

    /// `submit`: the key that sends typed text, for the keys line.
    fn content(
        &self,
        w: usize,
        t: &Theme,
        spaced: bool,
        submit: &str,
    ) -> (Vec<Line<'static>>, Option<(usize, usize)>, Sections) {
        let mut sections = Sections::default();
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
        // Line by line: a question's own line breaks (super's status lines) are kept.
        for l in q.question.lines() {
            out.extend(wrap::wrap(
                vec![Span::styled(l.to_string(), t.text.add_modifier(Modifier::BOLD))],
                w,
                &[],
                &[],
            ));
        }
        gap(&mut out);
        sections.context_end = out.len();
        let sel = self.sel[self.q];
        for (i, o) in q.options.iter().enumerate() {
            sections.choices.push(out.len());
            let focused = i == sel && !self.typing && !self.noting;
            let mark = if q.multi {
                if self.picked[self.q].contains(&i) {
                    "✓ "
                } else {
                    "○ "
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
            // The note key only where it works: on the selected choice.
            if focused {
                spans.push(Span::raw("  "));
                spans.extend(crate::hints::hints(&[("n", "note")], usize::MAX, t).spans);
            }
            let indent = [Span::raw(" ".repeat(6 + mark.chars().count()))];
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
        sections.choices.push(out.len());
        let typed = &self.typed[self.q];
        let on_text = sel == q.options.len();
        let lead = vec![
            Span::styled(if on_text && !self.typing { " ▸ " } else { "   " }, t.accent),
            Span::styled(format!("{}. ", q.options.len() + 1), t.dim),
        ];
        let style = if self.typing || on_text { t.accent } else { t.dim };
        if self.typing || !typed.is_empty() {
            cursor = field(&mut out, typed, lead, style, self.typing).or(cursor);
        } else {
            out.push(Line::from(
                [lead, vec![Span::styled("Type something…", style)]].concat(),
            ));
        }
        sections.choices.push(out.len());
        gap(&mut out);
        sections.preview_start = out.len();
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
        // Only keys that work now, most important first: a narrow form drops from the end.
        // The main key, then esc, so a narrow form always shows how to leave.
        let mut keys: Vec<(String, &str)> = Vec::new();
        let mut add = |k: &str, a: &'static str| keys.push((k.to_string(), a));
        if self.typing {
            add(submit, "answer");
            add("esc", "back");
        } else if self.noting {
            add(submit, if q.multi { "pick with note" } else { "choose with note" });
            add("esc", "back");
        } else {
            add(
                "enter",
                match (on_text, q.multi) {
                    (true, _) => "type",
                    (false, true) => "next",
                    (false, false) => "choose",
                },
            );
            add("esc", "dismiss");
            add("↑↓", "move");
            if q.multi && !on_text {
                add("space", "pick");
            }
            if self.q > 0 {
                add("←", "back");
            }
            if !q.multi {
                keys.push((format!("1-{}", self.rows().min(9)), "pick"));
            }
        }
        if !self.edit_key.is_empty() && (self.typing || self.noting || has_preview) {
            let what = if has_preview && !self.typing && !self.noting {
                "view/edit in editor"
            } else {
                "editor"
            };
            keys.insert(2.min(keys.len()), (self.edit_key.clone(), what));
        }
        sections.help_start = out.len();
        out.push(crate::hints::hints(&keys, w, t));
        (out, cursor, sections)
    }

    /// Rows the form takes, at most `max`: the spaced layout when it fits, else the
    /// compact one, the same choice [`AskForm::render`] makes.
    pub fn height(&self, width: u16, max: u16) -> u16 {
        let rows = |spaced| {
            self.lines(width.saturating_sub(4) as usize, &Theme::default(), spaced)
                .0
                .len() as u16
                + 2
        };
        let spaced = rows(true);
        if spaced <= max { spaced } else { rows(false).min(max) }
    }

    pub fn render(&self, f: &mut Frame, area: Rect, t: &Theme, km: &crate::keys::Keymap) {
        let submit = km.keys(A::Submit);
        let n = self.req.questions.len();
        let mut title = "Question".to_string();
        if n > 1 {
            title.push_str(&format!(" {} of {n}", self.q + 1));
        }
        if let Some(h) = &self.cur().header {
            title.push_str(&format!(" · {h}"));
        }
        let inner = crate::dialog::frame(f, area, &title, t);
        self.area.set(area);
        let w = inner.width as usize;
        let h = inner.height as usize;
        let mut content = self.content(w, t, true, &submit);
        if content.0.len() > h {
            content = self.content(w, t, false, &submit);
        }
        let (mut lines, mut cursor, sections) = content;
        // The choice a content row belongs to, for clicks.
        let choice_at = |row: usize| {
            let c = &sections.choices;
            (c.first().is_some_and(|&s| s <= row) && c.last().is_some_and(|&e| row < e))
                .then(|| c.partition_point(|&s| s <= row) - 1)
        };
        let mut tags: Vec<Option<usize>> = (0..lines.len()).map(choice_at).collect();
        self.visible_shortcuts.set(u16::MAX);
        self.rendered_selection.set(Some((self.q, self.sel[self.q])));
        self.context_max.set(0);
        if lines.len() > h {
            let help = lines.split_off(sections.help_start);
            let preview = lines.split_off(sections.preview_start);
            let controls = lines.split_off(sections.context_end);
            // Long cards keep controls and help fixed; overflow shares the context viewport.
            let budget = h.saturating_sub(help.len() + 1);
            let control_h = controls.len().min((budget * 2 / 3).max(1)).min(budget);
            let focus = sections.choices[self.sel[self.q]] - sections.context_end;
            let choice_end = sections
                .choices
                .get(self.sel[self.q] + 1)
                .map_or(controls.len(), |row| row - sections.context_end);
            let overflow = cursor.is_none() && choice_end - focus > control_h;
            if overflow {
                lines.extend(controls[focus + control_h..choice_end].iter().cloned());
            }
            lines.extend(preview);
            let context_h = budget.saturating_sub(control_h);
            let max = lines.len().saturating_sub(context_h);
            self.context_max.set(max);
            self.context_page.set(context_h.max(1));
            let top = self.context_scroll.get().min(max);
            self.context_scroll.set(top);
            let mut shown: Vec<_> = lines.into_iter().skip(top).take(context_h).collect();
            // Where the view is, then its key (PgUp/PgDn are matched as raw keys).
            let mut scroll = Line::styled(
                format!("{}–{} / {} · ", top + 1, top + context_h, max + context_h),
                t.dim,
            );
            let what = if overflow { "context/choice" } else { "context" };
            let room = w.saturating_sub(scroll.width());
            scroll
                .spans
                .extend(crate::hints::hints(&[("pgup/pgdn", what)], room, t).spans);
            shown.push(scroll);
            let target = cursor.map_or(
                choice_end.saturating_sub(1).min(focus + control_h.saturating_sub(1)),
                |(r, _)| r - sections.context_end,
            );
            let start = target
                .saturating_sub(control_h.saturating_sub(1))
                .min(controls.len().saturating_sub(control_h));
            // A long note still names the choice that Enter will send.
            let pinned = (focus < start && control_h > 1).then_some(focus);
            let start = start + usize::from(pinned.is_some());
            let end = (start + control_h - usize::from(pinned.is_some())).min(controls.len());
            let control_y = shown.len();
            tags = vec![None; control_y];
            if let Some(row) = pinned {
                shown.push(controls[row].clone());
                tags.push(choice_at(sections.context_end + row));
            }
            shown.extend(controls[start..end].iter().cloned());
            tags.extend((start..end).map(|row| choice_at(sections.context_end + row)));
            let visible = |row: usize| pinned == Some(row) || (start..end).contains(&row);
            if !visible(focus) {
                self.rendered_selection.set(Some((usize::MAX, usize::MAX)));
            }
            self.visible_shortcuts.set(
                sections
                    .choices
                    .windows(2)
                    .take(9)
                    .enumerate()
                    .fold(0, |mask, (i, rows)| {
                        let fully_visible = (rows[0]..rows[1]).all(|row| visible(row - sections.context_end));
                        mask | if fully_visible { 1 << i } else { 0 }
                    }),
            );
            cursor = cursor.and_then(|(r, c)| {
                let row = r - sections.context_end;
                (start..end)
                    .contains(&row)
                    .then(|| (control_y + usize::from(pinned.is_some()) + row - start, c))
            });
            shown.extend(help);
            lines = shown;
        }
        *self.choice_rows.borrow_mut() = tags
            .into_iter()
            .take(h)
            .enumerate()
            .filter_map(|(r, c)| Some((inner.y + r as u16, c?)))
            .collect();
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

    fn structured_form() -> AskForm {
        let (reply, _) = oneshot::channel();
        let mut question = String::new();
        for task in 1..=3 {
            question.push_str(&format!("Task {task}\n"));
            for heading in ["Context", "Evidence", "Decision", "Impact", "Recommendation"] {
                question.push_str(&format!("{heading}:\nTask {task} {heading} details\n\n"));
            }
        }
        AskForm::new(AskReq {
            id: 1,
            questions: vec![Question {
                question,
                header: Some("T1, T2, T3: question".into()),
                options: ["Approve", "Revise", "Wait"]
                    .into_iter()
                    .map(|label| Choice {
                        label: label.into(),
                        description: None,
                        preview: None,
                        recommended: false,
                    })
                    .collect(),
                multi: false,
            }],
            reply,
        })
    }

    fn screen(f: &AskForm, width: u16, height: u16) -> String {
        let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
        term.draw(|fr| {
            let area = Rect::new(0, 0, width, f.height(width, height * 2 / 3));
            f.render(fr, area, &Theme::default(), &Default::default());
        })
        .unwrap();
        let b = term.backend().buffer();
        (0..height)
            .map(|y| (0..width).map(|x| b[(x, y)].symbol()).collect::<String>() + "\n")
            .collect()
    }

    #[test]
    fn structured_context_scrolls_with_visible_answers() {
        for (width, height) in [(80, 24), (120, 40)] {
            let mut f = structured_form();
            let initial = screen(&f, width, height);
            for control in [
                "Task 1",
                "▸ 1. Approve",
                "2. Revise",
                "3. Wait",
                "Type something",
                "esc dismiss",
                "pgup/pgdn",
            ] {
                assert!(initial.contains(control), "missing {control}: {initial}");
            }
            let mut seen = initial;
            for _ in 0..60 {
                assert!(matches!(hit(&mut f, KeyCode::PageDown), Done::Pending));
                let shown = screen(&f, width, height);
                assert!(
                    shown.contains("▸ 1. Approve") && shown.contains("esc dismiss"),
                    "{shown}"
                );
                seen.push_str(&shown);
            }
            for task in 1..=3 {
                for heading in ["Context", "Evidence", "Decision", "Impact", "Recommendation"] {
                    assert!(seen.contains(&format!("Task {task} {heading} details")), "{seen}");
                }
            }
            for _ in 0..60 {
                hit(&mut f, KeyCode::PageUp);
            }
            assert!(screen(&f, width, height).contains("Task 1"));
            hit(&mut f, KeyCode::Down);
            assert!(screen(&f, width, height).contains("▸ 2. Revise"));
            let Done::Answered(replies) = hit(&mut f, KeyCode::Enter) else {
                panic!("not answered")
            };
            assert_eq!(replies[0].chosen, ["Revise"]);
        }
    }

    #[test]
    fn structured_context_keeps_typed_answer_and_note_visible() {
        let mut f = structured_form();
        hit(&mut f, KeyCode::Char('n'));
        f.paste("with tests");
        assert!(screen(&f, 80, 24).contains("note: with tests"));
        hit(&mut f, KeyCode::Esc);
        hit(&mut f, KeyCode::Up);
        assert!(screen(&f, 80, 24).contains("Type something"));
        hit(&mut f, KeyCode::Enter);
        f.paste("my answer");
        let shown = screen(&f, 80, 24);
        assert!(shown.contains("my answer") && shown.contains("enter answer"), "{shown}");
        let Done::Answered(replies) = hit(&mut f, KeyCode::Enter) else {
            panic!("not answered")
        };
        assert_eq!(replies[0].text.as_deref(), Some("my answer"));
    }

    #[test]
    fn hidden_number_shortcut_focuses_before_confirming() {
        let mut f = structured_form();
        for option in &mut f.req.questions[0].options {
            option.description = Some("Description wraps across many rows. ".repeat(20));
        }
        let initial = screen(&f, 80, 24);
        assert!(initial.contains("▸ 1. Approve"));
        assert!(!initial.contains("3. Wait"));
        assert!(matches!(hit(&mut f, KeyCode::Char('3')), Done::Pending));
        assert!(
            matches!(hit(&mut f, KeyCode::Enter), Done::Pending),
            "must draw the new selection first"
        );
        let shown = screen(&f, 80, 24);
        assert!(shown.contains("▸ 3. Wait") && shown.contains("esc dismiss"), "{shown}");
        let Done::Answered(replies) = hit(&mut f, KeyCode::Enter) else {
            panic!("not answered")
        };
        assert_eq!(replies[0].chosen, ["Wait"]);
    }

    #[test]
    fn partially_visible_number_shortcut_only_focuses() {
        for multi in [false, true] {
            let mut f = structured_form();
            f.req.questions[0].multi = multi;
            f.req.questions[0].options[1].label = format!("{}LabelEnd", "Wrapped choice ".repeat(40));
            f.req.questions[0].options[1].description = Some("Hidden description".into());
            let initial = screen(&f, 80, 24);
            assert!(
                initial.contains("2.") && initial.contains("Wrapped choice"),
                "{initial}"
            );
            assert!(
                !initial.contains("LabelEnd") && !initial.contains("Hidden description"),
                "{initial}"
            );
            assert!(matches!(hit(&mut f, KeyCode::Char('2')), Done::Pending));
            assert_eq!(f.sel[0], 1);
            assert!(f.picked[0].is_empty());
            assert!(matches!(hit(&mut f, KeyCode::Enter), Done::Pending));
            screen(&f, 80, 24);
            let mut seen = String::new();
            for _ in 0..100 {
                hit(&mut f, KeyCode::PageDown);
                seen.push_str(&screen(&f, 80, 24));
            }
            assert!(
                seen.contains("LabelEnd") && seen.contains("Hidden description"),
                "{seen}"
            );
            let Done::Answered(replies) = hit(&mut f, KeyCode::Enter) else {
                panic!("not answered")
            };
            assert_eq!(replies[0].chosen, [f.req.questions[0].options[1].label.clone()]);
        }
        let mut f = structured_form();
        screen(&f, 80, 24);
        let Done::Answered(replies) = hit(&mut f, KeyCode::Char('2')) else {
            panic!("fully visible choice should submit immediately")
        };
        assert_eq!(replies[0].chosen, ["Revise"]);
    }

    #[test]
    fn later_choice_shows_wrapped_label_and_description() {
        let mut f = structured_form();
        f.req.questions[0].options[1].label = format!("{}label end", "Later choice ".repeat(7));
        f.req.questions[0].options[1].description = Some("Description end".into());
        screen(&f, 80, 24);
        hit(&mut f, KeyCode::Down);
        let shown = screen(&f, 80, 24);
        for text in ["▸ 2. Later choice", "label end", "Description end", "esc dismiss"] {
            assert!(shown.contains(text), "missing {text}: {shown}");
        }
    }

    #[test]
    fn later_choice_overflow_is_readable_by_paging() {
        for width in [40, 80] {
            let mut f = structured_form();
            f.req.questions[0].options[2].label = "Long label ".repeat(30);
            f.req.questions[0].options[2].description = Some((0..80).map(|i| format!("Detail-{i:02} ")).collect());
            f.req.questions[0].options[2].preview = Some("Preview end".into());
            screen(&f, width, 24);
            hit(&mut f, KeyCode::Down);
            hit(&mut f, KeyCode::Down);
            let (lines, _, sections) = f.content(width as usize - 4, &Theme::default(), false, "enter");
            let expected = &lines[sections.choices[2]..sections.choices[3]];
            let mut seen = screen(&f, width, 24);
            assert!(seen.contains("context/choice"), "{seen}");
            for _ in 0..100 {
                hit(&mut f, KeyCode::PageDown);
                let shown = screen(&f, width, 24);
                assert!(
                    shown.contains("▸ 3. Long label") && shown.contains("esc dismiss"),
                    "{shown}"
                );
                seen.push_str(&shown);
            }
            for row in expected {
                let text = wrap::plain(row);
                assert!(seen.contains(text.trim()), "missing {text}: {seen}");
            }
            assert!(seen.contains("Preview end"), "{seen}");
            for _ in 0..100 {
                hit(&mut f, KeyCode::PageUp);
            }
            assert!(screen(&f, width, 24).contains("Task 1"));
            let Done::Answered(replies) = hit(&mut f, KeyCode::Enter) else {
                panic!("not answered")
            };
            assert_eq!(replies[0].chosen, [f.req.questions[0].options[2].label.clone()]);
        }
    }

    #[test]
    fn scrolling_preserves_preview_note_and_question_back() {
        let mut f = structured_form();
        f.req.questions[0].options[0].preview = Some("Preview first\nPreview last".into());
        let second = f.req.questions[0].clone();
        // Build both questions together so their editor and selection state is initialized.
        let (reply, _) = oneshot::channel();
        let mut f = AskForm::new(AskReq {
            id: 1,
            questions: vec![f.req.questions[0].clone(), second],
            reply,
        });
        screen(&f, 80, 24);
        for _ in 0..60 {
            hit(&mut f, KeyCode::PageDown);
            screen(&f, 80, 24);
        }
        let shown = screen(&f, 80, 24);
        assert!(
            shown.contains("Preview last") && shown.contains("▸ 1. Approve"),
            "{shown}"
        );
        hit(&mut f, KeyCode::Char('n'));
        f.paste(&"long note ".repeat(100));
        f.paste("note end");
        let shown = screen(&f, 80, 24);
        assert!(
            shown.contains("1. Approve") && shown.contains("note end") && shown.contains("enter choose with note"),
            "{shown}"
        );
        assert!(matches!(hit(&mut f, KeyCode::Enter), Done::Pending));
        assert!(screen(&f, 80, 24).contains("Task 1"));
        hit(&mut f, KeyCode::Left);
        let shown = screen(&f, 80, 24);
        assert!(
            shown.contains("Question 1 of 2") && shown.contains("Task 1") && shown.contains("▸ 1. Approve"),
            "{shown}"
        );
    }

    #[test]
    fn question_line_breaks_are_kept() {
        let (tx, _rx) = oneshot::channel();
        let f = AskForm::new(AskReq {
            id: 1,
            questions: vec![Question {
                question: "T1 · ready\nWhere things stand: ok\n\nStart T2?".into(),
                header: None,
                options: vec![],
                multi: false,
            }],
            reply: tx,
        });
        let text: Vec<String> = f
            .lines(60, &Theme::default(), true)
            .0
            .iter()
            .map(crate::wrap::plain)
            .collect();
        assert_eq!(
            &text[..4],
            ["T1 · ready", "Where things stand: ok", "", "Start T2?"],
            "{text:?}"
        );
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
            term.draw(|fr| f.render(fr, fr.area(), &Theme::default(), &Default::default()))
                .unwrap();
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
        let tall = draw(f.height(60, u16::MAX));
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
        let short = draw(f.height(60, u16::MAX) - 4);
        assert!(short.iter().any(|l| l.contains("esc dismiss")), "{short:?}");
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
