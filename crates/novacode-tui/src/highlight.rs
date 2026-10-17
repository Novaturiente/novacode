//! Code-block highlighting with syntect (L9.6, feature `highlight`). The
//! grammars load once on first use; finished turns are laid out once and
//! cached, so a block is highlighted once per width.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;
use std::sync::OnceLock;
use syntect::easy::HighlightLines;
use syntect::highlighting::{FontStyle, ThemeSet};
use syntect::parsing::SyntaxSet;
use syntect::util::LinesWithEndings;

/// Bigger blocks stay plain: highlighting them would stall a frame.
const MAX_BYTES: usize = 200_000;

fn sets() -> &'static (SyntaxSet, ThemeSet) {
    static S: OnceLock<(SyntaxSet, ThemeSet)> = OnceLock::new();
    S.get_or_init(|| (SyntaxSet::load_defaults_newlines(), ThemeSet::load_defaults()))
}

/// One span list per line, or `None` for an unknown language.
pub fn lines(code: &str, lang: &str, theme: &str, ansi256: bool) -> Option<Vec<Vec<Span<'static>>>> {
    if code.len() > MAX_BYTES {
        return None;
    }
    let (ss, ts) = sets();
    let token = lang.split([',', ' ', '{']).next().unwrap_or(lang);
    let syntax = ss.find_syntax_by_token(token)?;
    let theme = ts.themes.get(theme).or_else(|| ts.themes.values().next())?;
    let mut h = HighlightLines::new(syntax, theme);
    let mut out = Vec::new();
    for line in LinesWithEndings::from(code) {
        let ranges = h.highlight_line(line, ss).ok()?;
        out.push(
            ranges
                .into_iter()
                .filter_map(|(st, text)| {
                    let text = text.trim_end_matches(['\n', '\r']);
                    if text.is_empty() {
                        return None;
                    }
                    let c = Color::Rgb(st.foreground.r, st.foreground.g, st.foreground.b);
                    let mut s = Style::new().fg(if ansi256 { crate::theme::to_256(c) } else { c });
                    if st.font_style.contains(FontStyle::BOLD) {
                        s = s.add_modifier(Modifier::BOLD);
                    }
                    if st.font_style.contains(FontStyle::ITALIC) {
                        s = s.add_modifier(Modifier::ITALIC);
                    }
                    Some(Span::styled(text.to_string(), s))
                })
                .collect(),
        );
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    #[test]
    fn highlights_known_languages_only() {
        let l = super::lines("fn main() {}\nlet x = 1;\n", "rust", "base16-ocean.dark", false).unwrap();
        assert_eq!(l.len(), 2);
        assert!(l[0].len() > 1, "keywords get their own span");
        assert_eq!(
            l[0].iter().map(|s| s.content.as_ref()).collect::<String>(),
            "fn main() {}"
        );
        assert!(super::lines("x", "no-such-lang", "base16-ocean.dark", false).is_none());
    }
}
