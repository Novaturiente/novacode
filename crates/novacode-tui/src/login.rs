//! The `/login` dialog: the sign-in URL with keys to copy or open it, a device
//! code, or a box for a pasted code / redirect URL / API key. The host runs the
//! login and sends [`LoginStep`]s; what the user types goes back as input.

use crate::theme::Theme;
use crate::wrap;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

/// What a running login shows.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "step", rename_all = "snake_case")]
pub enum LoginStep {
    /// Open `url`; `paste` = a pasted code or redirect URL is taken too.
    Url { url: String, note: String, paste: bool },
    /// Enter `code` at `url`; novacode waits for the approval.
    Device { url: String, code: String },
    /// Type a secret (an API key); shown masked.
    Key { note: String },
    /// Progress or a problem to fix (e.g. a key that failed its check).
    Info { text: String },
}

pub struct LoginView {
    pub provider: String,
    url: Option<String>,
    code: Option<String>,
    note: String,
    info: Option<String>,
    /// `None` = nothing to type in this step.
    input: Option<String>,
    secret: bool,
    error: Option<String>,
}

pub enum LoginKey {
    None,
    Copy(String),
    Open(String),
    Submit(String),
    Close,
}

impl LoginView {
    pub fn new(provider: &str) -> Self {
        Self {
            provider: provider.into(),
            url: None,
            code: None,
            note: "starting…".into(),
            info: None,
            input: None,
            secret: false,
            error: None,
        }
    }

    pub fn step(&mut self, s: LoginStep) {
        match s {
            LoginStep::Url { url, note, paste } => {
                self.url = Some(url);
                self.note = note;
                self.input = paste.then(String::new);
            }
            LoginStep::Device { url, code } => {
                self.url = Some(url);
                self.note = "Enter this code on the page, then approve. novacode waits for it.".into();
                self.code = Some(code);
                self.input = None;
            }
            LoginStep::Key { note } => {
                self.note = note;
                self.input = Some(String::new());
                self.secret = true;
            }
            LoginStep::Info { text } => self.info = Some(text),
        }
    }

    pub fn failed(&mut self, e: String) {
        self.error = Some(e);
        self.input = None;
    }

    pub fn paste(&mut self, s: &str) {
        if let Some(i) = &mut self.input {
            i.push_str(s.trim());
        }
    }

    pub fn handle(&mut self, k: KeyEvent, copy_key: bool) -> LoginKey {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        match k.code {
            KeyCode::Esc => return LoginKey::Close,
            KeyCode::Char('c') if ctrl => return LoginKey::Close,
            _ if copy_key => {
                if let Some(u) = &self.url {
                    self.info = Some("URL copied to the clipboard".into());
                    return LoginKey::Copy(u.clone());
                }
            }
            KeyCode::Char('o') if ctrl => {
                if let Some(u) = &self.url {
                    self.info = Some("opened in your browser".into());
                    return LoginKey::Open(u.clone());
                }
            }
            KeyCode::Enter if self.error.is_some() => return LoginKey::Close,
            KeyCode::Enter => {
                if let Some(i) = &mut self.input
                    && !i.trim().is_empty()
                {
                    self.info = Some("checking…".into());
                    return LoginKey::Submit(std::mem::take(i).trim().to_string());
                }
            }
            KeyCode::Backspace => {
                if let Some(i) = &mut self.input {
                    i.pop();
                }
            }
            KeyCode::Char(c) if !ctrl => {
                if let Some(i) = &mut self.input {
                    i.push(c);
                }
            }
            _ => {}
        }
        LoginKey::None
    }

    fn lines(&self, w: usize, t: &Theme, copy_key: &str) -> Vec<Line<'static>> {
        let mut out = Vec::new();
        let text = |s: &str, st| wrap::wrap(vec![Span::styled(s.to_string(), st)], w, &[], &[]);
        out.extend(text(&self.note, t.text));
        if let Some(u) = &self.url {
            out.push(Line::default());
            // Cut into rows by hand: a URL has no spaces to wrap at.
            let chars: Vec<char> = u.chars().collect();
            for row in chars.chunks(w.max(10)) {
                out.push(Line::styled(row.iter().collect::<String>(), t.accent));
            }
        }
        if let Some(c) = &self.code {
            out.push(Line::default());
            out.push(Line::styled(format!("    {c}"), t.accent.add_modifier(Modifier::BOLD)));
        }
        if let Some(i) = &self.input {
            out.push(Line::default());
            let shown = if self.secret {
                "•".repeat(i.chars().count())
            } else {
                i.clone()
            };
            let hint = if self.secret {
                "API key"
            } else {
                "paste the code or the URL the browser ends on"
            };
            out.push(wrap::truncate(
                vec![
                    Span::styled("› ", t.accent),
                    Span::styled(format!("{shown}▏"), t.text),
                    Span::styled(if i.is_empty() { hint } else { "" }, t.dim),
                ],
                w,
            ));
        }
        if let Some(e) = &self.error {
            out.push(Line::default());
            out.extend(text(e, t.error));
        } else if let Some(i) = &self.info {
            out.push(Line::default());
            out.extend(text(i, t.dim));
        }
        out.push(Line::default());
        let mut keys = Vec::new();
        if self.url.is_some() {
            keys.push(format!("{copy_key} copy URL"));
            keys.push("Ctrl+O open in browser".into());
        }
        if self.input.is_some() {
            keys.push("Enter submit".into());
        }
        keys.push(
            if self.error.is_some() {
                "Esc close"
            } else {
                "Esc cancel"
            }
            .into(),
        );
        out.extend(text(&keys.join(" · "), t.dim));
        out
    }

    pub fn height(&self, width: u16) -> u16 {
        self.lines(width.saturating_sub(4) as usize, &Theme::default(), "Ctrl+X")
            .len() as u16
            + 2
    }

    pub fn render(&self, f: &mut Frame, area: Rect, t: &Theme, copy_key: &str) {
        let inner = crate::dialog::frame(f, area, &format!("Log in to {}", self.provider), t);
        f.render_widget(Paragraph::new(self.lines(inner.width as usize, t, copy_key)), inner);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(c: KeyCode, m: KeyModifiers) -> KeyEvent {
        KeyEvent::new(c, m)
    }

    #[test]
    fn url_copy_open_and_paste() {
        let mut v = LoginView::new("claude");
        v.step(LoginStep::Url {
            url: "https://claude.ai/oauth/authorize?x=1".into(),
            note: "Sign in".into(),
            paste: true,
        });
        assert!(
            matches!(v.handle(key(KeyCode::Char('x'), KeyModifiers::CONTROL), true), LoginKey::Copy(u) if u.contains("claude.ai"))
        );
        assert!(matches!(
            v.handle(key(KeyCode::Char('o'), KeyModifiers::CONTROL), false),
            LoginKey::Open(_)
        ));
        // Plain letters go to the paste box, even `o`.
        v.paste(" abc#");
        v.handle(key(KeyCode::Char('o'), KeyModifiers::NONE), false);
        assert!(
            matches!(v.handle(key(KeyCode::Enter, KeyModifiers::NONE), false), LoginKey::Submit(s) if s == "abc#o")
        );
        let shown: String = v
            .lines(30, &Theme::default(), "Ctrl+X")
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
            .collect();
        assert!(
            shown.contains("https://claude.ai/oauth/autho"),
            "URL cut into rows: {shown}"
        );
        assert!(shown.contains("Ctrl+X copy URL"));
        v.failed("bad code".into());
        assert!(matches!(
            v.handle(key(KeyCode::Enter, KeyModifiers::NONE), false),
            LoginKey::Close
        ));
    }

    #[test]
    fn key_is_masked() {
        let mut v = LoginView::new("groq");
        v.step(LoginStep::Key {
            note: "Paste your key".into(),
        });
        v.paste("sk-123");
        let shown: String = v
            .lines(40, &Theme::default(), "Ctrl+X")
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
            .collect();
        assert!(!shown.contains("sk-123") && shown.contains("••••••"));
    }
}
