//! One-off fullscreen choices before the session starts: the trust prompt
//! (L6.3) and first-run onboarding (L11.6). Blocking; runs before the main UI.

use crate::dialog::{PickItem, Picked, Picker};
use crate::keys::Keymap;
use crate::term;
use crate::theme::Theme;
use crate::wrap;
use crossterm::event::{self, Event, KeyEventKind};
use ratatui::layout::{Constraint, Layout};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

/// Show `body` above a picker; `None` = cancelled.
pub fn select(title: &str, body: &[String], items: Vec<PickItem>) -> std::io::Result<Option<String>> {
    term::install_panic_hook();
    let mut t = term::enter()?;
    let theme = Theme::default();
    let km = Keymap::default();
    let mut p = Picker::new(title, items);
    let res = loop {
        t.draw(|f| {
            let area = f.area();
            let lines: Vec<Line> = body
                .iter()
                .flat_map(|l| {
                    wrap::wrap(
                        vec![Span::styled(l.clone(), theme.text)],
                        area.width as usize - 2,
                        &[Span::raw(" ")],
                        &[Span::raw(" ")],
                    )
                })
                .collect();
            let [top, rest] =
                Layout::vertical([Constraint::Length(lines.len() as u16 + 1), Constraint::Min(3)]).areas(area);
            f.render_widget(Paragraph::new(lines), top);
            p.render(f, rest, &theme);
        })?;
        match event::read()? {
            Event::Key(k) if k.kind != KeyEventKind::Release => match p.handle(k, &km) {
                Picked::Pending => {}
                Picked::Cancel => break None,
                Picked::Value(v) => break Some(v),
            },
            _ => {}
        }
    };
    term::restore();
    Ok(res)
}
