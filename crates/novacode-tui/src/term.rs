//! Terminal setup and restore (L9.2). Restore runs on exit, on panic and
//! around Ctrl+Z, so the shell is never left in raw mode.

use crossterm::event::{
    DisableBracketedPaste, DisableFocusChange, DisableMouseCapture, EnableBracketedPaste, EnableFocusChange,
    EnableMouseCapture, KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::terminal::{self, EnterAlternateScreen, LeaveAlternateScreen};
use crossterm::{execute, queue};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use std::io::{Stdout, Write, stdout};
use std::sync::atomic::{AtomicBool, Ordering};

pub type Term = Terminal<CrosstermBackend<Stdout>>;

static KITTY: AtomicBool = AtomicBool::new(false);

pub fn enter() -> std::io::Result<Term> {
    terminal::enable_raw_mode()?;
    let mut out = stdout();
    execute!(
        out,
        EnterAlternateScreen,
        EnableBracketedPaste,
        EnableMouseCapture,
        EnableFocusChange
    )?;
    // Kitty keyboard protocol: Shift+Enter, Ctrl+I vs Tab and friends are distinguishable.
    if terminal::supports_keyboard_enhancement().unwrap_or(false) {
        execute!(
            out,
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
        )?;
        KITTY.store(true, Ordering::Relaxed);
    }
    let mut t = Terminal::new(CrosstermBackend::new(out))?;
    t.clear()?;
    Ok(t)
}

/// Safe to call more than once.
pub fn restore() {
    let mut out = stdout();
    if KITTY.swap(false, Ordering::Relaxed) {
        let _ = queue!(out, PopKeyboardEnhancementFlags);
    }
    let _ = queue!(
        out,
        DisableFocusChange,
        DisableMouseCapture,
        DisableBracketedPaste,
        LeaveAlternateScreen,
        crossterm::cursor::Show
    );
    let _ = out.flush();
    let _ = terminal::disable_raw_mode();
}

/// Restore the terminal before the default panic message prints.
pub fn install_panic_hook() {
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore();
        tracing::error!(%info, "panic");
        prev(info);
    }));
}

/// Ctrl+Z: give the terminal back, stop, and take it again on `fg`.
pub fn suspend(t: &mut Term) -> std::io::Result<()> {
    restore();
    let _ = nix::sys::signal::raise(nix::sys::signal::Signal::SIGTSTP);
    *t = enter()?;
    Ok(())
}

/// Ctrl+G: edit `text` in `$VISUAL`/`$EDITOR` (default `vi`). `None` if unchanged.
pub fn external_editor(t: &mut Term, text: &str) -> std::io::Result<Option<String>> {
    let editor = std::env::var("VISUAL")
        .or_else(|_| std::env::var("EDITOR"))
        .unwrap_or_else(|_| "vi".into());
    let path = std::env::temp_dir().join(format!("novacode-prompt-{}.md", std::process::id()));
    std::fs::write(&path, text)?;
    restore();
    // Through sh, so `EDITOR="code --wait"` works; the path is passed as $1, never spliced.
    let status = std::process::Command::new("sh")
        .arg("-c")
        .arg(format!("{editor} \"$1\""))
        .arg("sh")
        .arg(&path)
        .status();
    *t = enter()?;
    let out = std::fs::read_to_string(&path);
    let _ = std::fs::remove_file(&path);
    if !status?.success() {
        return Ok(None);
    }
    let new = out?.trim_end_matches('\n').to_string();
    Ok((new != text).then_some(new))
}

/// Copy to the clipboard: OSC 52 (works over SSH and in tmux with
/// set-clipboard on), plus wl-copy when on Wayland.
pub fn copy(text: &str) {
    use base64::Engine;
    let b64 = base64::engine::general_purpose::STANDARD.encode(text);
    let mut out = stdout();
    let _ = write!(out, "\x1b]52;c;{b64}\x07");
    let _ = out.flush();
    if std::env::var_os("WAYLAND_DISPLAY").is_some()
        && let Ok(mut child) = std::process::Command::new("wl-copy")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
    {
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(text.as_bytes());
        }
        // Reap in the background; wl-copy forks and exits quickly.
        std::thread::spawn(move || child.wait());
    }
}
