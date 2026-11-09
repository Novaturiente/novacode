//! Key hints (docs/tui-style.md "Key hints"): `key action · key action`, dim, one line.
//! Every screen draws its hints here. Callers pass key labels from the keymap
//! (`Keymap::keys`), so rebound keys show the user's key, and list only keys that work now.

use crate::theme::Theme;
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

const SEP: &str = " · ";

/// (key, action) pairs in `w` cells. Pairs that don't fit are dropped from the end,
/// so put the most important first. Never wraps. A pair with no key (an unbound action)
/// is skipped.
pub fn hints<K: AsRef<str>, V: AsRef<str>>(pairs: &[(K, V)], w: usize, t: &Theme) -> Line<'static> {
    let mut text = String::new();
    for (k, a) in pairs.iter().filter(|(k, _)| !k.as_ref().is_empty()) {
        let item = format!("{} {}", k.as_ref(), a.as_ref());
        let sep = if text.is_empty() { "" } else { SEP };
        if text.width() + sep.width() + item.width() > w {
            break;
        }
        text.push_str(sep);
        text.push_str(&item);
    }
    Line::from(Span::styled(text, t.dim))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pairs_join_with_dots_and_drop_from_the_end() {
        let t = Theme::default();
        let pairs = [("↑↓", "move"), ("enter", "open"), ("esc", "back")];
        let text = |w| {
            hints(&pairs, w, &t)
                .spans
                .iter()
                .map(|s| s.content.to_string())
                .collect::<String>()
        };
        assert_eq!(text(80), "↑↓ move · enter open · esc back");
        // "↑↓ move · enter open" is 20 cells: the last hint goes, never a cut one.
        assert_eq!(text(29), "↑↓ move · enter open");
        assert_eq!(text(20), "↑↓ move · enter open");
        assert_eq!(text(19), "↑↓ move");
        assert_eq!(text(3), "");
        let unbound = hints(&[("", "send"), ("esc", "back")], 80, &t);
        assert_eq!(unbound.spans[0].content, "esc back", "an unbound key is skipped");
        let line = hints(&pairs, 80, &t);
        assert!(line.spans.iter().all(|s| s.style == t.dim), "every hint is dim");
    }
}
