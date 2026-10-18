//! The prompt editor (L9.5): multiline with soft wrap, grapheme and word
//! moves, kill ring, undo, history and collapsed paste markers.

use crate::keys::{A, Keymap, Scope};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

const UNDO_CAP: usize = 200;
const PASTE_LINES: usize = 5;
const PASTE_CHARS: usize = 1000;

#[derive(Default)]
pub struct Editor {
    text: String,
    /// Byte offset, always on a grapheme boundary.
    cursor: usize,
    kill: Vec<String>,
    undo: Vec<(String, usize)>,
    last_edit: Option<EditKind>,
    pastes: Vec<String>,
    history: Vec<String>,
    /// Position while browsing history, and the draft it replaced.
    hist: Option<(usize, String)>,
    /// Ctrl+R: query, matched history index, draft to restore on Esc.
    search: Option<(String, Option<usize>, String)>,
    /// The last yank's range and kill-ring index, for yank-pop.
    yanked: Option<(usize, usize, usize)>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum EditKind {
    Insert,
    Delete,
    Other,
}

pub enum Action {
    None,
    Submit(String),
    /// Alt+Enter: queue as a follow-up instead of steering.
    FollowUp(String),
}

/// One visual row: byte range of `text`.
struct Row {
    start: usize,
    end: usize,
}

impl Editor {
    pub fn with_history(history: Vec<String>) -> Self {
        Self {
            history,
            ..Default::default()
        }
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    pub fn clear(&mut self) {
        self.save(EditKind::Other);
        self.text.clear();
        self.cursor = 0;
        self.pastes.clear();
        self.hist = None;
    }

    fn save(&mut self, kind: EditKind) {
        // Consecutive typing or deleting is one undo step.
        if self.last_edit != Some(kind) || kind == EditKind::Other {
            self.undo.push((self.text.clone(), self.cursor));
            if self.undo.len() > UNDO_CAP {
                self.undo.remove(0);
            }
        }
        self.last_edit = Some(kind);
    }

    pub fn insert(&mut self, s: &str) {
        self.save(EditKind::Insert);
        self.text.insert_str(self.cursor, s);
        self.cursor += s.len();
    }

    /// Bracketed paste: big pastes collapse into a marker, expanded on submit.
    pub fn paste(&mut self, s: &str) {
        let s = s.replace("\r\n", "\n").replace('\r', "\n");
        let lines = s.lines().count();
        if lines > PASTE_LINES || s.len() > PASTE_CHARS {
            self.pastes.push(s);
            let marker = format!("[paste #{} +{lines} lines]", self.pastes.len());
            self.save(EditKind::Other);
            self.text.insert_str(self.cursor, &marker);
            self.cursor += marker.len();
        } else {
            self.save(EditKind::Other);
            self.text.insert_str(self.cursor, &s);
            self.cursor += s.len();
        }
    }

    fn prev(&self, i: usize) -> usize {
        self.text[..i].grapheme_indices(true).next_back().map_or(0, |(j, _)| j)
    }

    fn next(&self, i: usize) -> usize {
        self.text[i..].graphemes(true).next().map_or(i, |g| i + g.len())
    }

    fn word_left(&self, i: usize) -> usize {
        let before = &self.text[..i];
        let trimmed = before.trim_end_matches(|c: char| !c.is_alphanumeric() && c != '_');
        trimmed
            .rfind(|c: char| !c.is_alphanumeric() && c != '_')
            .map_or(0, |j| j + trimmed[j..].chars().next().unwrap().len_utf8())
    }

    fn word_right(&self, i: usize) -> usize {
        let after = &self.text[i..];
        let skip = after.len()
            - after
                .trim_start_matches(|c: char| !c.is_alphanumeric() && c != '_')
                .len();
        let rest = &after[skip..];
        i + skip
            + rest
                .find(|c: char| !c.is_alphanumeric() && c != '_')
                .unwrap_or(rest.len())
    }

    fn line_start(&self) -> usize {
        self.text[..self.cursor].rfind('\n').map_or(0, |j| j + 1)
    }

    fn line_end(&self) -> usize {
        self.text[self.cursor..]
            .find('\n')
            .map_or(self.text.len(), |j| self.cursor + j)
    }

    fn delete(&mut self, a: usize, b: usize, kill: bool) {
        if a >= b {
            return;
        }
        self.save(EditKind::Delete);
        let gone: String = self.text.drain(a..b).collect();
        if kill {
            self.kill.push(gone);
            if self.kill.len() > 20 {
                self.kill.remove(0);
            }
        }
        self.cursor = a;
    }

    /// A paste marker right before the cursor, deleted as one unit.
    fn marker_before(&self) -> Option<usize> {
        let before = &self.text[..self.cursor];
        if !before.ends_with(" lines]") {
            return None;
        }
        let start = before.rfind("[paste #")?;
        let inner = &before[start + 8..before.len() - 7];
        let (n, rest) = inner.split_once(" +")?;
        (n.parse::<usize>().is_ok() && rest.parse::<usize>().is_ok()).then_some(start)
    }

    fn history(&mut self, up: bool) {
        if self.history.is_empty() {
            return;
        }
        let (idx, draft) = match self.hist.take() {
            Some(h) => h,
            None if up => (self.history.len(), self.text.clone()),
            None => return,
        };
        let idx = if up {
            idx.saturating_sub(1)
        } else if idx + 1 >= self.history.len() {
            self.text = draft;
            self.cursor = self.text.len();
            return;
        } else {
            idx + 1
        };
        self.text = self.history[idx].clone();
        self.cursor = self.text.len();
        self.hist = Some((idx, draft));
    }

    /// Visual rows for `width` cells (grapheme wrap; one cell kept for the cursor).
    fn rows(&self, width: usize) -> Vec<Row> {
        let width = width.max(2) - 1;
        let mut rows = Vec::new();
        let mut start = 0;
        for line in self.text.split('\n') {
            let base = start;
            let mut row_start = base;
            let mut w = 0;
            for (i, g) in line.grapheme_indices(true) {
                let gw = g.width();
                if w + gw > width {
                    rows.push(Row {
                        start: row_start,
                        end: base + i,
                    });
                    row_start = base + i;
                    w = 0;
                }
                w += gw;
            }
            rows.push(Row {
                start: row_start,
                end: base + line.len(),
            });
            start = base + line.len() + 1;
        }
        rows
    }

    /// (row, column in cells) of the cursor.
    fn cursor_pos(&self, rows: &[Row]) -> (usize, usize) {
        let r = rows.iter().rposition(|r| r.start <= self.cursor).unwrap_or(0);
        (r, self.text[rows[r].start..self.cursor].width())
    }

    fn at_col(&self, row: &Row, col: usize) -> usize {
        let mut w = 0;
        for (i, g) in self.text[row.start..row.end].grapheme_indices(true) {
            if w + g.width() > col {
                return row.start + i;
            }
            w += g.width();
        }
        row.end
    }

    fn vertical(&mut self, up: bool, width: usize) {
        let rows = self.rows(width);
        let (r, col) = self.cursor_pos(&rows);
        let target = if up {
            r.checked_sub(1)
        } else {
            (r + 1 < rows.len()).then_some(r + 1)
        };
        match target {
            Some(t) => self.cursor = self.at_col(&rows[t], col),
            None => self.history(up),
        }
    }

    /// The input with collapsed pastes put back (what submitting would send).
    pub fn full_text(&self) -> String {
        let mut out = self.text.clone();
        for (i, p) in self.pastes.iter().enumerate().rev() {
            let marker = format!("[paste #{} +{} lines]", i + 1, p.lines().count());
            out = out.replace(&marker, p);
        }
        out
    }

    fn submit_text(&mut self) -> String {
        let out = self.full_text();
        self.text.clear();
        self.clear();
        self.undo.clear();
        if !out.trim().is_empty() && self.history.last() != Some(&out) {
            self.history.push(out.clone());
        }
        out
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// Replace `start..cursor` with `s` (completion).
    pub fn complete(&mut self, start: usize, s: &str) {
        self.save(EditKind::Other);
        self.text.replace_range(start..self.cursor, s);
        self.cursor = start + s.len();
    }

    /// Replace the whole text (from `$EDITOR` or a picked history entry).
    pub fn set_text(&mut self, s: &str) {
        self.save(EditKind::Other);
        self.text = s.to_string();
        self.cursor = self.text.len();
    }

    /// Take the text for sending (paste markers expanded, added to history). `None` if blank.
    pub fn take(&mut self) -> Option<String> {
        (!self.text.trim().is_empty()).then(|| self.submit_text())
    }

    pub fn searching(&self) -> bool {
        self.search.is_some()
    }

    /// Ctrl+R: start a reverse history search, or jump to the next older match.
    pub fn search_history(&mut self) {
        match &mut self.search {
            None => self.search = Some((String::new(), None, self.text.clone())),
            Some((q, at, _)) => {
                let q = q.clone();
                let from = at.unwrap_or(self.history.len());
                *at = self.history[..from].iter().rposition(|h| h.contains(&q)).or(*at);
            }
        }
        self.show_match();
    }

    fn show_match(&mut self) {
        if let Some((_, Some(i), _)) = &self.search {
            self.text = self.history[*i].clone();
            self.cursor = self.text.len();
        }
    }

    fn search_key(&mut self, k: KeyEvent) {
        let Some((q, at, draft)) = &mut self.search else { return };
        match k.code {
            KeyCode::Esc => {
                self.text = std::mem::take(draft);
                self.cursor = self.text.len();
                self.search = None;
            }
            KeyCode::Char('g') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                self.text = std::mem::take(draft);
                self.cursor = self.text.len();
                self.search = None;
            }
            KeyCode::Backspace => {
                q.pop();
                *at = self.history.iter().rposition(|h| h.contains(q.as_str()));
                self.show_match();
            }
            KeyCode::Char(c) if !k.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) => {
                q.push(c);
                *at = self.history.iter().rposition(|h| h.contains(q.as_str()));
                self.show_match();
            }
            // Anything else accepts the match and is handled normally.
            _ => self.search = None,
        }
    }

    /// The search prompt shown above the editor, if searching.
    pub fn search_prompt(&self) -> Option<String> {
        self.search.as_ref().map(|(q, at, _)| {
            let state = if at.is_some() || q.is_empty() {
                ""
            } else {
                " (no match)"
            };
            format!("history search: {q}{state}   Ctrl+R older · Enter use · Esc cancel")
        })
    }

    /// `a` is the bound action for `k`, if any (see [`Keymap::lookup`]).
    pub fn handle(&mut self, a: Option<A>, k: KeyEvent, width: usize) -> Action {
        if self.search.is_some() {
            // Enter only accepts the match; it doesn't send.
            if k.code == KeyCode::Enter {
                self.search = None;
                return Action::None;
            }
            let cancel =
                k.code == KeyCode::Esc || (k.code == KeyCode::Char('g') && k.modifiers.contains(KeyModifiers::CONTROL));
            self.search_key(k);
            // Still searching, or cancelled: done. Any other key accepted the match and runs below.
            if self.search.is_some() || cancel {
                return Action::None;
            }
        }
        if !matches!(a, Some(A::CursorUp | A::CursorDown)) {
            self.hist = None;
        }
        if a != Some(A::YankPop) && a != Some(A::Yank) {
            self.yanked = None;
        }
        match a {
            Some(A::NewLine) => self.insert("\n"),
            Some(A::Submit) => {
                if let Some(t) = self.take() {
                    return Action::Submit(t);
                }
            }
            Some(A::LineStart) => self.cursor = self.line_start(),
            Some(A::LineEnd) => self.cursor = self.line_end(),
            Some(A::CursorLeft) => self.cursor = self.prev(self.cursor),
            Some(A::CursorRight) => self.cursor = self.next(self.cursor),
            Some(A::WordLeft) => self.cursor = self.word_left(self.cursor),
            Some(A::WordRight) => self.cursor = self.word_right(self.cursor),
            Some(A::CursorUp) => self.vertical(true, width),
            Some(A::CursorDown) => self.vertical(false, width),
            Some(A::DeleteToLineEnd) => {
                let end = self.line_end();
                let end = if end == self.cursor && end < self.text.len() {
                    end + 1
                } else {
                    end
                };
                self.delete(self.cursor, end, true);
            }
            Some(A::DeleteToLineStart) => self.delete(self.line_start(), self.cursor, true),
            Some(A::DeleteWordBack) => self.delete(self.word_left(self.cursor), self.cursor, true),
            Some(A::DeleteWordForward) => self.delete(self.cursor, self.word_right(self.cursor), true),
            Some(A::DeleteBack) => match self.marker_before() {
                Some(start) => self.delete(start, self.cursor, false),
                None => self.delete(self.prev(self.cursor), self.cursor, false),
            },
            Some(A::DeleteForward) => self.delete(self.cursor, self.next(self.cursor), false),
            Some(A::Yank) => {
                if let Some(s) = self.kill.last().cloned() {
                    self.save(EditKind::Other);
                    self.text.insert_str(self.cursor, &s);
                    self.yanked = Some((self.cursor, self.cursor + s.len(), self.kill.len() - 1));
                    self.cursor += s.len();
                }
            }
            Some(A::YankPop) => {
                if let Some((start, end, i)) = self.yanked
                    && self.kill.len() > 1
                {
                    let j = if i == 0 { self.kill.len() - 1 } else { i - 1 };
                    let s = self.kill[j].clone();
                    self.text.replace_range(start..end, &s);
                    self.cursor = start + s.len();
                    self.yanked = Some((start, self.cursor, j));
                }
            }
            Some(A::Undo) => {
                if let Some((t, c)) = self.undo.pop() {
                    self.text = t;
                    self.cursor = c.min(self.text.len());
                    self.last_edit = None;
                }
            }
            Some(_) => {}
            None => {
                if let KeyCode::Char(c) = k.code
                    && !k.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
                {
                    let mut b = [0; 4];
                    self.insert(c.encode_utf8(&mut b));
                }
            }
        }
        if !matches!(a, None | Some(A::DeleteBack | A::DeleteForward)) {
            self.last_edit = None;
        }
        Action::None
    }

    /// Raw key with the default keymap (tests, simple callers).
    pub fn handle_key(&mut self, k: KeyEvent, width: usize) -> Action {
        let a = Keymap::default().lookup(&k, &[Scope::Editor]);
        self.handle(a, k, width)
    }

    /// Visible rows (at most `max`) and the cursor (row, col) inside them.
    pub fn view(&self, width: usize, max: usize) -> (Vec<String>, (u16, u16)) {
        let rows = self.rows(width);
        let (r, c) = self.cursor_pos(&rows);
        let first = (r + 1).saturating_sub(max);
        let shown: Vec<String> = rows[first..]
            .iter()
            .take(max)
            .map(|row| self.text[row.start..row.end].to_string())
            .collect();
        (shown, ((r - first) as u16, c as u16))
    }

    /// Rows needed at `width`, for the layout.
    pub fn height(&self, width: usize) -> usize {
        self.rows(width).len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(e: &mut Editor, code: KeyCode, m: KeyModifiers) -> Action {
        e.handle_key(KeyEvent::new(code, m), 20)
    }
    fn typ(e: &mut Editor, s: &str) {
        for c in s.chars() {
            key(e, KeyCode::Char(c), KeyModifiers::NONE);
        }
    }

    #[test]
    fn editing_kill_yank_undo() {
        let mut e = Editor::default();
        typ(&mut e, "hello brave world");
        key(&mut e, KeyCode::Char('w'), KeyModifiers::CONTROL);
        assert_eq!(e.text(), "hello brave ");
        key(&mut e, KeyCode::Left, KeyModifiers::CONTROL);
        assert_eq!(e.cursor, 6);
        key(&mut e, KeyCode::Char('k'), KeyModifiers::CONTROL);
        assert_eq!(e.text(), "hello ");
        key(&mut e, KeyCode::Char('y'), KeyModifiers::CONTROL);
        assert_eq!(e.text(), "hello brave ");
        key(&mut e, KeyCode::Char('_'), KeyModifiers::CONTROL);
        assert_eq!(e.text(), "hello ");
        // Typing a run is one undo step.
        typ(&mut e, "xyz");
        key(&mut e, KeyCode::Char('_'), KeyModifiers::CONTROL);
        assert_eq!(e.text(), "hello ");
        typ(&mut e, "é😀");
        key(&mut e, KeyCode::Backspace, KeyModifiers::NONE);
        assert_eq!(e.text(), "hello é");
    }

    #[test]
    fn newline_submit_history() {
        let mut e = Editor::default();
        typ(&mut e, "one");
        key(&mut e, KeyCode::Enter, KeyModifiers::SHIFT);
        typ(&mut e, "two");
        let Action::Submit(s) = key(&mut e, KeyCode::Enter, KeyModifiers::NONE) else {
            panic!()
        };
        assert_eq!(s, "one\ntwo");
        assert!(e.is_empty());
        typ(&mut e, "draft");
        key(&mut e, KeyCode::Up, KeyModifiers::NONE);
        assert_eq!(e.text(), "one\ntwo");
        key(&mut e, KeyCode::Down, KeyModifiers::NONE);
        assert_eq!(e.text(), "draft");
        // Empty input never submits.
        e.clear();
        assert!(matches!(key(&mut e, KeyCode::Enter, KeyModifiers::NONE), Action::None));
    }

    #[test]
    fn soft_wrap_and_vertical_moves() {
        let mut e = Editor::default();
        typ(&mut e, &"a".repeat(45));
        let (rows, cur) = e.view(20, 10);
        assert_eq!(rows.len(), 3);
        assert_eq!(cur, (2, 7));
        key(&mut e, KeyCode::Up, KeyModifiers::NONE);
        assert_eq!(e.cursor, 26);
        assert_eq!(e.view(20, 2).0.len(), 2);
    }

    #[test]
    fn paste_markers() {
        let mut e = Editor::default();
        typ(&mut e, "see ");
        e.paste(&"line\n".repeat(10));
        assert_eq!(e.text(), "see [paste #1 +10 lines]");
        let Action::Submit(s) = key(&mut e, KeyCode::Enter, KeyModifiers::NONE) else {
            panic!()
        };
        assert_eq!(s, format!("see {}", "line\n".repeat(10)));
        typ(&mut e, "x ");
        e.paste(&"y\n".repeat(8));
        key(&mut e, KeyCode::Backspace, KeyModifiers::NONE);
        assert_eq!(e.text(), "x ");
    }
}
