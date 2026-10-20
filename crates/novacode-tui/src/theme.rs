//! Semantic styles from JSON themes (L9.11): catppuccin mocha (dark) and
//! latte (light) are bundled; `~/.novacode/themes/<name>.json` adds more.
//! A user theme only needs the tokens it changes; the rest come from the
//! bundled theme of the same brightness.

use ratatui::style::{Color, Modifier, Style};
use serde::Deserialize;
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::Path;

const MOCHA: &str = include_str!("../themes/catppuccin-mocha.json");
const LATTE: &str = include_str!("../themes/catppuccin-latte.json");

pub struct Theme {
    pub name: String,
    pub dark: bool,
    /// syntect theme for code blocks.
    pub syntax: String,
    pub text: Style,
    pub dim: Style,
    pub accent: Style,
    pub user: Style,
    pub heading: Style,
    pub code: Style,
    pub code_block: Style,
    pub link: Style,
    pub quote: Style,
    pub thinking: Style,
    pub ok: Style,
    pub warn: Style,
    pub error: Style,
    pub add: Style,
    pub del: Style,
    pub border: Style,
    pub selection: Style,
    pub toggle: Style,
    /// The current transcript search match.
    pub matched: Style,
    /// Editor border per thinking level, off → max (pi's `thinkingOff`…`thinkingMax`).
    pub levels: [Style; 7],
    /// Downgrade truecolor to the 256-colour palette.
    pub ansi256: bool,
    /// How prompts are drawn in the transcript (setting `ui.userMessage`).
    pub user_box: UserBox,
}

/// The frame around a prompt in the transcript.
#[derive(Debug, Clone, PartialEq)]
pub struct UserBox {
    /// Corner and edge glyphs; `None` = no frame.
    pub border: Option<[char; 6]>,
    pub border_style: Style,
    /// Text colours; a `bg` fills the whole box.
    pub text: Style,
    pub pad_x: usize,
    pub pad_y: usize,
}

/// tl, tr, bl, br, horizontal, vertical.
const BORDERS: &[(&str, [char; 6])] = &[
    ("rounded", ['╭', '╮', '╰', '╯', '─', '│']),
    ("single", ['┌', '┐', '└', '┘', '─', '│']),
    ("heavy", ['┏', '┓', '┗', '┛', '━', '┃']),
    ("double", ['╔', '╗', '╚', '╝', '═', '║']),
    ("ascii", ['+', '+', '+', '+', '-', '|']),
];

#[derive(Deserialize)]
struct File {
    #[serde(default)]
    name: String,
    #[serde(default)]
    dark: Option<bool>,
    #[serde(default)]
    syntax: Option<String>,
    #[serde(default)]
    tokens: BTreeMap<String, Value>,
}

impl Default for Theme {
    fn default() -> Self {
        Self::bundled(true, false)
    }
}

fn hex(s: &str) -> Option<Color> {
    let s = s.strip_prefix('#')?;
    if s.len() != 6 {
        return None;
    }
    let n = u32::from_str_radix(s, 16).ok()?;
    Some(Color::Rgb((n >> 16) as u8, (n >> 8) as u8, n as u8))
}

/// Nearest xterm-256 colour (6×6×6 cube or grey ramp).
pub fn to_256(c: Color) -> Color {
    let Color::Rgb(r, g, b) = c else { return c };
    let cube = |v: u8| if v < 48 { 0 } else if v < 115 { 1 } else { (v as u16 - 35) / 40 } as u8;
    let level = |i: u8| if i == 0 { 0 } else { 55 + 40 * i as u16 } as i32;
    let (cr, cg, cb) = (cube(r), cube(g), cube(b));
    let d = |a: (i32, i32, i32)| (a.0 - r as i32).pow(2) + (a.1 - g as i32).pow(2) + (a.2 - b as i32).pow(2);
    let cube_d = d((level(cr), level(cg), level(cb)));
    let avg = (r as i32 + g as i32 + b as i32) / 3;
    let gi = ((avg - 8).max(0) / 10).min(23);
    let gv = 8 + 10 * gi;
    if d((gv, gv, gv)) < cube_d {
        Color::Indexed(232 + gi as u8)
    } else {
        Color::Indexed(16 + 36 * cr + 6 * cg + cb)
    }
}

fn style(v: &Value) -> Option<Style> {
    match v {
        Value::String(s) => Some(Style::new().fg(hex(s)?)),
        Value::Object(o) => {
            let mut st = Style::new();
            if let Some(c) = o.get("fg").and_then(Value::as_str).and_then(hex) {
                st = st.fg(c);
            }
            if let Some(c) = o.get("bg").and_then(Value::as_str).and_then(hex) {
                st = st.bg(c);
            }
            for (k, m) in [
                ("bold", Modifier::BOLD),
                ("italic", Modifier::ITALIC),
                ("underline", Modifier::UNDERLINED),
                ("dim", Modifier::DIM),
            ] {
                if o.get(k).and_then(Value::as_bool) == Some(true) {
                    st = st.add_modifier(m);
                }
            }
            Some(st)
        }
        _ => None,
    }
}

impl Theme {
    fn bundled(dark: bool, ansi256: bool) -> Self {
        let f: File = serde_json::from_str(if dark { MOCHA } else { LATTE }).expect("bundled theme");
        Self::build(f, None, ansi256)
    }

    /// `over` tokens win over `base` ones; bad values are skipped and reported by [`Theme::load`].
    fn build(base: File, over: Option<File>, ansi256: bool) -> Self {
        let mut tokens = base.tokens;
        let mut name = base.name;
        let mut syntax = base.syntax.unwrap_or_default();
        let dark = over.as_ref().and_then(|o| o.dark).or(base.dark).unwrap_or(true);
        if let Some(o) = over {
            name = o.name;
            if let Some(s) = o.syntax {
                syntax = s;
            }
            tokens.extend(o.tokens);
        }
        let get = |k: &str| {
            let st = tokens.get(k).and_then(style).unwrap_or_default();
            if ansi256 {
                Style {
                    fg: st.fg.map(to_256),
                    bg: st.bg.map(to_256),
                    ..st
                }
            } else {
                st
            }
        };
        Self {
            name,
            dark,
            syntax,
            text: get("text"),
            dim: get("dim"),
            accent: get("accent"),
            user: get("user"),
            heading: get("heading"),
            code: get("code"),
            code_block: get("code_block"),
            link: get("link"),
            quote: get("quote"),
            thinking: get("thinking"),
            ok: get("ok"),
            warn: get("warn"),
            error: get("error"),
            add: get("add"),
            del: get("del"),
            border: get("border"),
            selection: get("selection"),
            toggle: get("toggle"),
            matched: get("match"),
            levels: ["off", "minimal", "low", "medium", "high", "xhigh", "max"].map(|l| get(&format!("thinking_{l}"))),
            ansi256,
            user_box: UserBox {
                border: Some(BORDERS[0].1),
                border_style: get("text"),
                text: get("text"),
                pad_x: 1,
                pad_y: 0,
            },
        }
    }

    /// Apply the `ui.userMessage` setting:
    /// `{ "border": "rounded|single|heavy|double|ascii|none", "borderColor": "#hex" | {fg,bold,…},
    ///    "text": "#hex" | {fg,bg,bold,italic,…}, "paddingX": 1, "paddingY": 0 }`.
    /// Returns a warning for values it could not use.
    /// A style by token name (`accent`, `dim`, …) or `#rrggbb`, for extension UI. Unknown → text.
    pub fn token(&self, name: Option<&str>) -> Style {
        match name.unwrap_or("text") {
            "dim" => self.dim,
            "accent" => self.accent,
            "user" => self.user,
            "heading" => self.heading,
            "code" => self.code,
            "link" => self.link,
            "thinking" => self.thinking,
            "ok" | "success" => self.ok,
            "warn" | "warning" => self.warn,
            "error" => self.error,
            "add" => self.add,
            "del" => self.del,
            "border" => self.border,
            n if n.starts_with('#') => match hex(n) {
                Some(c) => Style::default().fg(if self.ansi256 { to_256(c) } else { c }),
                None => self.text,
            },
            _ => self.text,
        }
    }

    pub fn apply_user_message(&mut self, v: &Value) -> Option<String> {
        let mut bad = Vec::new();
        let fix = |st: Style| {
            if self.ansi256 {
                Style {
                    fg: st.fg.map(to_256),
                    bg: st.bg.map(to_256),
                    ..st
                }
            } else {
                st
            }
        };
        let b = &mut self.user_box;
        if let Some(name) = v.get("border") {
            match name.as_str() {
                Some("none") => b.border = None,
                Some(n) if BORDERS.iter().any(|(k, _)| *k == n) => {
                    b.border = BORDERS.iter().find(|(k, _)| *k == n).map(|(_, g)| *g)
                }
                _ => bad.push("border"),
            }
        }
        for (key, slot) in [("borderColor", &mut b.border_style), ("text", &mut b.text)] {
            if let Some(x) = v.get(key) {
                match style(x) {
                    Some(st) => *slot = fix(st),
                    None => bad.push(key),
                }
            }
        }
        for (key, slot) in [("paddingX", &mut b.pad_x), ("paddingY", &mut b.pad_y)] {
            if let Some(x) = v.get(key) {
                match x.as_u64() {
                    Some(n) if n <= 8 => *slot = n as usize,
                    _ => bad.push(key),
                }
            }
        }
        (!bad.is_empty()).then(|| format!("settings ui.userMessage: ignored bad {}", bad.join(", ")))
    }

    /// `name`: `auto` (by terminal background), `dark`, `light`, a bundled name, or a file in `dir`.
    /// `dark_bg` is the detected background (None = unknown → dark).
    pub fn load(name: &str, dir: &Path, dark_bg: Option<bool>) -> (Self, Option<String>) {
        let ansi256 = !truecolor();
        let auto_dark = dark_bg.unwrap_or(true);
        match name {
            "" | "auto" => return (Self::bundled(auto_dark, ansi256), None),
            "dark" | "catppuccin-mocha" => return (Self::bundled(true, ansi256), None),
            "light" | "catppuccin-latte" => return (Self::bundled(false, ansi256), None),
            _ => {}
        }
        let path = dir.join(format!("{name}.json"));
        let parsed = std::fs::read_to_string(&path)
            .map_err(|e| e.to_string())
            .and_then(|s| serde_json::from_str::<File>(&s).map_err(|e| e.to_string()));
        match parsed {
            Ok(mut f) => {
                if f.name.is_empty() {
                    f.name = name.into();
                }
                let bad: Vec<String> = f
                    .tokens
                    .iter()
                    .filter(|(_, v)| style(v).is_none())
                    .map(|(k, _)| k.clone())
                    .collect();
                f.tokens.retain(|_, v| style(v).is_some());
                let base_dark = f.dark.unwrap_or(auto_dark);
                let base: File = serde_json::from_str(if base_dark { MOCHA } else { LATTE }).expect("bundled theme");
                let warn = (!bad.is_empty()).then(|| format!("theme {name}: ignored bad tokens {}", bad.join(", ")));
                (Self::build(base, Some(f), ansi256), warn)
            }
            Err(e) => (
                Self::bundled(auto_dark, ansi256),
                Some(format!(
                    "theme `{name}` not loaded ({}: {e}); using the default",
                    path.display()
                )),
            ),
        }
    }
}

fn truecolor() -> bool {
    std::env::var("COLORTERM").is_ok_and(|v| v == "truecolor" || v == "24bit")
        || std::env::var("TERM").is_ok_and(|t| t.contains("kitty") || t.contains("ghostty") || t.contains("direct"))
}

/// Parse an OSC 11 reply like `\x1b]11;rgb:1e1e/1e1e/2e2e\x07`: true = dark.
pub fn dark_from_osc11(reply: &str) -> Option<bool> {
    let rgb = reply.split("rgb:").nth(1)?;
    let parts: Vec<u32> = rgb
        .split(['/', '\x07', '\x1b'])
        .take(3)
        .map(|p| u32::from_str_radix(&p[..p.len().min(2)], 16).ok())
        .collect::<Option<_>>()?;
    let [r, g, b] = parts[..] else { return None };
    let luma = 0.2126 * r as f64 + 0.7152 * g as f64 + 0.0722 * b as f64;
    Some(luma < 128.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_theme_overrides_some_tokens() {
        let d = std::env::temp_dir().join(format!("nc-theme-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(
            d.join("mine.json"),
            r##"{"dark": false, "tokens": {"accent": "#ff0000", "ok": 7}}"##,
        )
        .unwrap();
        let (t, warn) = Theme::load("mine", &d, Some(true));
        assert!(!t.dark);
        if !t.ansi256 {
            assert_eq!(t.accent.fg, Some(Color::Rgb(255, 0, 0)));
            // Unset tokens come from latte, the light base.
            assert_eq!(t.text.fg, Some(Color::Rgb(0x4c, 0x4f, 0x69)));
        }
        assert_eq!(warn.as_deref(), Some("theme mine: ignored bad tokens ok"));
        let (_, warn) = Theme::load("missing", &d, None);
        assert!(warn.unwrap().contains("not loaded"));
    }

    #[test]
    fn osc11_and_256() {
        assert_eq!(dark_from_osc11("\x1b]11;rgb:1e1e/1e1e/2e2e\x07"), Some(true));
        assert_eq!(dark_from_osc11("\x1b]11;rgb:efef/f1f1/f5f5\x1b\\"), Some(false));
        assert_eq!(dark_from_osc11("junk"), None);
        assert_eq!(to_256(Color::Rgb(255, 0, 0)), Color::Indexed(196));
        assert_eq!(to_256(Color::Rgb(30, 30, 30)), Color::Indexed(234));
    }
}
