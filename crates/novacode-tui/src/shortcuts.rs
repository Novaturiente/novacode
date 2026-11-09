//! The shortcut sheet (G2, TP7): `?` on an empty input shows the common keys in two
//! columns, with labels from the keymap, and points to the full list.

use crate::keys::{A, Keymap};
use crate::theme::Theme;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use unicode_width::UnicodeWidthStr;

/// The actions shown, most used first, with short labels (3 words or fewer).
const ITEMS: &[(A, &str)] = &[
    (A::Interrupt, "interrupt"),
    (A::NewLine, "newline"),
    (A::ModelSelect, "pick model"),
    (A::ThinkingCycle, "thinking level"),
    (A::ThinkingToggle, "show thinking"),
    (A::ToolsExpand, "full tool output"),
    (A::TurnsToggle, "fold turns"),
    (A::HistorySearch, "search history"),
    (A::Search, "search chat"),
    (A::ExternalEditor, "external editor"),
    (A::MessageCopy, "copy reply"),
    (A::PanelToggle, "side panel"),
];

const GAP: usize = 3;

/// Draw the sheet centred in `area`.
pub fn render(f: &mut Frame, area: Rect, km: &Keymap, t: &Theme) {
    let items: Vec<(String, &str)> = ITEMS
        .iter()
        .map(|&(a, label)| (km.keys(a), label))
        .filter(|(k, _)| !k.is_empty())
        .collect();
    let rows = items.len().div_ceil(2);
    let (left, right) = items.split_at(rows);
    let width = |col: &[(String, &str)]| {
        let kw = col.iter().map(|(k, _)| k.width()).max().unwrap_or(0);
        let lw = col.iter().map(|(_, l)| l.width()).max().unwrap_or(0);
        (kw, kw + 1 + lw)
    };
    let ((lk, lw), (rk, rw)) = (width(left), width(right));
    let cell = |(k, l): &(String, &str), kw: usize| {
        vec![
            Span::styled(format!("{k:<kw$} "), t.accent),
            Span::styled(l.to_string(), t.text),
        ]
    };
    let mut lines: Vec<Line> = (0..rows)
        .map(|i| {
            let mut s = cell(&left[i], lk);
            if let Some(r) = right.get(i) {
                s.push(Span::raw(" ".repeat(lw - (lk + 1 + left[i].1.width()) + GAP)));
                s.extend(cell(r, rk));
            }
            Line::from(s)
        })
        .collect();
    let content = lw + GAP + rw;
    let mut more = vec![("any key", "close")];
    let help = km.keys(A::KeysHelp);
    if !help.is_empty() {
        more.insert(0, (help.as_str(), "all keys"));
    }
    lines.push(Line::default());
    lines.push(crate::hints::hints(&more, content, t));
    // Frame plus one cell of padding each side.
    let w = (content as u16 + 4).min(area.width);
    let h = (lines.len() as u16 + 2).min(area.height);
    let r = Rect {
        x: area.x + (area.width - w) / 2,
        y: area.y + (area.height - h) / 2,
        width: w,
        height: h,
    };
    let inner = crate::dialog::frame(f, r, "Shortcuts", t);
    f.render_widget(Paragraph::new(lines), inner);
}
