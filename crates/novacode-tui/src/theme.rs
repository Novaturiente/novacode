//! Semantic styles (L9.11). M4a ships catppuccin mocha only.
// shortcut: no theme files, light theme or 256-colour fallback yet (M4b).

use ratatui::style::{Color, Modifier, Style};

pub struct Theme {
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
}

const fn rgb(r: u8, g: u8, b: u8) -> Style {
    Style::new().fg(Color::Rgb(r, g, b))
}

impl Default for Theme {
    fn default() -> Self {
        Self {
            text: rgb(0xcd, 0xd6, 0xf4),
            dim: rgb(0x7f, 0x84, 0x9c),
            accent: rgb(0xcb, 0xa6, 0xf7),
            user: rgb(0x89, 0xb4, 0xfa).add_modifier(Modifier::BOLD),
            heading: rgb(0xf5, 0xc2, 0xe7).add_modifier(Modifier::BOLD),
            code: rgb(0xfa, 0xb3, 0x87),
            code_block: rgb(0xa6, 0xe3, 0xa1),
            link: rgb(0x74, 0xc7, 0xec).add_modifier(Modifier::UNDERLINED),
            quote: rgb(0x93, 0x99, 0xb2).add_modifier(Modifier::ITALIC),
            thinking: rgb(0x93, 0x99, 0xb2).add_modifier(Modifier::ITALIC),
            ok: rgb(0xa6, 0xe3, 0xa1),
            warn: rgb(0xf9, 0xe2, 0xaf),
            error: rgb(0xf3, 0x8b, 0xa8),
            add: rgb(0xa6, 0xe3, 0xa1),
            del: rgb(0xf3, 0x8b, 0xa8),
            border: rgb(0x58, 0x5b, 0x70),
            selection: Style::new().bg(Color::Rgb(0x45, 0x47, 0x5a)),
            toggle: rgb(0x94, 0xe2, 0xd5),
        }
    }
}
