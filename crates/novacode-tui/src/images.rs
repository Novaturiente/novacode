//! Images in the transcript (L9.14): Kitty, iTerm2 or Sixel when the terminal
//! answers the query, else half-blocks. Without the `images` feature, every
//! image stays a one-line `[image W×H]` placeholder.

/// Pixel size from the image header (no full decode).
#[cfg(feature = "images")]
pub fn dimensions(b64: &str) -> Option<(u32, u32)> {
    use base64::Engine;
    let bytes = base64::engine::general_purpose::STANDARD.decode(b64).ok()?;
    image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .ok()?
        .into_dimensions()
        .ok()
}

#[cfg(not(feature = "images"))]
pub fn dimensions(_: &str) -> Option<(u32, u32)> {
    None
}

#[cfg(feature = "images")]
pub use with::Images;

#[cfg(feature = "images")]
mod with {
    use crate::transcript::Img;
    use ratatui::Frame;
    use ratatui::layout::{Rect, Size};
    use ratatui_image::picker::Picker;
    use ratatui_image::protocol::Protocol;
    use ratatui_image::{Image, Resize};
    use std::collections::HashMap;

    pub struct Images {
        picker: Picker,
        /// Encoded once per (image, area size).
        cache: HashMap<(u64, u16, u16), Option<Protocol>>,
    }

    impl Images {
        /// The protocol from the `ui.images` setting or the environment. No stdin
        /// query: ratatui-image's leaves a thread reading stdin when the terminal
        /// doesn't answer, and it eats the user's keys. `None` = images off.
        #[allow(deprecated)] // from_fontsize: the query-free constructor
        pub fn detect(setting: &str) -> Option<Self> {
            use ratatui_image::picker::ProtocolType as P;
            let env = |k: &str| std::env::var(k).unwrap_or_default();
            let term = env("TERM");
            let program = env("TERM_PROGRAM");
            let proto = match setting {
                "off" => return None,
                "kitty" => P::Kitty,
                "iterm2" => P::Iterm2,
                "sixel" => P::Sixel,
                "halfblocks" => P::Halfblocks,
                _ if !env("KITTY_WINDOW_ID").is_empty() || term == "xterm-kitty" => P::Kitty,
                _ if program == "ghostty" || !env("GHOSTTY_RESOURCES_DIR").is_empty() => P::Kitty,
                _ if !env("KONSOLE_VERSION").is_empty() => P::Kitty,
                _ if !env("WEZTERM_EXECUTABLE").is_empty() || program == "iTerm.app" || program == "WezTerm" => {
                    P::Iterm2
                }
                _ if term.starts_with("foot") || term.contains("mlterm") => P::Sixel,
                _ => P::Halfblocks,
            };
            // Cell size in pixels from the tty (TIOCGWINSZ), when the terminal reports it.
            let font = crossterm::terminal::window_size()
                .ok()
                .filter(|w| w.width > 0 && w.columns > 0 && w.rows > 0)
                .map(|w| ratatui_image::FontSize::new(w.width / w.columns, w.height / w.rows))
                .unwrap_or(ratatui_image::FontSize::new(10, 20));
            let mut picker = Picker::from_fontsize(font);
            picker.set_protocol_type(proto);
            Some(Images {
                picker,
                cache: HashMap::new(),
            })
        }

        pub fn halfblocks() -> Self {
            Images {
                picker: Picker::halfblocks(),
                cache: HashMap::new(),
            }
        }

        pub fn render(&mut self, f: &mut Frame, area: Rect, img: &Img) {
            let key = (img.key, area.width, area.height);
            if self.cache.len() > 64 {
                self.cache.clear();
            }
            let picker = &self.picker;
            let p = self.cache.entry(key).or_insert_with(|| {
                use base64::Engine;
                let bytes = base64::engine::general_purpose::STANDARD
                    .decode(img.data.as_str())
                    .ok()?;
                let dynimg = image::load_from_memory(&bytes).ok()?;
                picker
                    .new_protocol(dynimg, Size::new(area.width, area.height), Resize::Fit(None))
                    .ok()
            });
            if let Some(p) = p {
                f.render_widget(Image::new(p), area);
            }
        }
    }
}
