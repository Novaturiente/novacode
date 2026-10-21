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

/// Is the terminal background dark? Asks with OSC 11 (150 ms timeout), then
/// falls back to `COLORFGBG`. Call before [`enter`]; `None` = unknown.
pub fn query_background() -> Option<bool> {
    use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
    use std::io::Read;
    use std::os::fd::AsFd;
    let from_env = || {
        let v = std::env::var("COLORFGBG").ok()?;
        let bg: u8 = v.rsplit(';').next()?.parse().ok()?;
        Some(!matches!(bg, 7 | 15))
    };
    // On reload the TUI's terminal is still up (raw): leave it that way afterwards.
    let was_raw = terminal::is_raw_mode_enabled().unwrap_or(false);
    if terminal::enable_raw_mode().is_err() {
        return from_env();
    }
    let mut out = stdout();
    let _ = write!(out, "\x1b]11;?\x07");
    let _ = out.flush();
    let stdin = std::io::stdin();
    let mut reply = Vec::new();
    let mut buf = [0u8; 64];
    while reply.len() < 256 {
        let mut fds = [PollFd::new(stdin.as_fd(), PollFlags::POLLIN)];
        if !matches!(poll(&mut fds, PollTimeout::from(150u16)), Ok(n) if n > 0) {
            break;
        }
        match stdin.lock().read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => reply.extend_from_slice(&buf[..n]),
        }
        if reply.ends_with(b"\x07") || reply.ends_with(b"\x1b\\") {
            break;
        }
    }
    if !was_raw {
        let _ = terminal::disable_raw_mode();
    }
    crate::theme::dark_from_osc11(&String::from_utf8_lossy(&reply)).or_else(from_env)
}

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
    } else {
        // tmux ignores the kitty push but honours xterm modifyOtherKeys (with
        // `extended-keys on`), which still gets Shift+Enter through as CSI 13;2u.
        execute!(out, crossterm::style::Print("\x1b[>4;1m"))?;
    }
    // Not `Terminal::clear()`: it queries the cursor position and waits on stdin for the
    // reply, which a still-draining event reader of the previous run can swallow (2 s
    // timeout, then exit) after /clone, /rewind, /fork. Fullscreen needs no cursor.
    execute!(out, terminal::Clear(terminal::ClearType::All))?;
    let t = Terminal::new(CrosstermBackend::new(out))?;
    Ok(t)
}

/// Safe to call more than once.
pub fn restore() {
    let mut out = stdout();
    if KITTY.swap(false, Ordering::Relaxed) {
        let _ = queue!(out, PopKeyboardEnhancementFlags);
    } else {
        let _ = queue!(out, crossterm::style::Print("\x1b[>4m"));
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

/// The editor to run: `$VISUAL`, `$EDITOR`, else the first of nvim, nano, emacs, vim, vi on PATH.
pub fn editor_command() -> String {
    let set = |k| std::env::var(k).ok().filter(|v: &String| !v.trim().is_empty());
    set("VISUAL").or_else(|| set("EDITOR")).unwrap_or_else(|| {
        let path = std::env::var_os("PATH").unwrap_or_default();
        let found = |b: &&&str| std::env::split_paths(&path).any(|d| d.join(b).is_file());
        match ["nvim", "nano", "emacs", "vim"].iter().find(found) {
            // In the terminal, not a GUI window.
            Some(&"emacs") => "emacs -nw".into(),
            Some(b) => b.to_string(),
            None => "vi".into(),
        }
    })
}

/// Ctrl+G: edit `text` in [`editor_command`]. `None` if unchanged.
pub fn external_editor(t: &mut Term, text: &str) -> std::io::Result<Option<String>> {
    let editor = editor_command();
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

/// Files tab: open `path` in the editor. With `base` (its content when the session
/// started) and an editor that diffs (nvim, vim, vi), both open side by side.
pub fn open_in_editor(t: &mut Term, path: &std::path::Path, base: Option<&std::path::Path>) -> std::io::Result<()> {
    let editor = editor_command();
    let bin = editor.split_whitespace().next().unwrap_or_default();
    let bin = bin.rsplit('/').next().unwrap_or(bin);
    let diff = base.filter(|_| matches!(bin, "nvim" | "vim" | "vi" | "gvim"));
    restore();
    // Through sh, so `EDITOR="code --wait"` works; paths are passed as $1/$2, never spliced.
    let mut c = std::process::Command::new("sh");
    match diff {
        Some(b) => c
            .arg("-c")
            .arg(format!("{editor} -d \"$1\" \"$2\""))
            .arg("sh")
            .arg(b)
            .arg(path),
        None => c.arg("-c").arg(format!("{editor} \"$1\"")).arg("sh").arg(path),
    };
    let status = c.status();
    *t = enter()?;
    status.map(|_| ())
}

/// Open a URL in the default browser (xdg-open, or `open` on macOS); output goes nowhere.
pub fn open(url: &str) {
    let opener = if cfg!(target_os = "macos") { "open" } else { "xdg-open" };
    let _ = std::process::Command::new(opener)
        .arg(url)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
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

pub enum Pasted {
    /// A clipboard image saved to a temp file; the prompt mentions it as `@path`.
    Image(std::path::PathBuf),
    Text(String),
}

/// Read the clipboard (Ctrl+V, L2.22): an image if it has one, else text.
/// Wayland via wl-paste, X11 via xclip; every call is capped at 2s.
pub fn paste() -> Option<Pasted> {
    let wayland = std::env::var_os("WAYLAND_DISPLAY").is_some();
    let types = if wayland {
        run("wl-paste", &["--list-types"])?
    } else {
        run("xclip", &["-selection", "clipboard", "-t", "TARGETS", "-o"])?
    };
    let types = String::from_utf8_lossy(&types);
    if let Some(t) = image_type(&types) {
        let bytes = if wayland {
            run("wl-paste", &["--type", t])
        } else {
            run("xclip", &["-selection", "clipboard", "-t", t, "-o"])
        }?;
        let ext = t.strip_prefix("image/").unwrap_or("png").replace("jpeg", "jpg");
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .ok()?
            .as_millis();
        let path = std::env::temp_dir().join(format!("novacode-clip-{}-{stamp}.{ext}", std::process::id()));
        std::fs::write(&path, bytes).ok()?;
        return Some(Pasted::Image(path));
    }
    let text = if wayland {
        run("wl-paste", &["--no-newline"])
    } else {
        run("xclip", &["-selection", "clipboard", "-o"])
    }?;
    Some(Pasted::Text(String::from_utf8_lossy(&text).into_owned()))
}

/// The best image type on offer: formats providers take, before ones to convert.
fn image_type(types: &str) -> Option<&str> {
    let types: Vec<&str> = types.lines().map(str::trim).collect();
    ["image/png", "image/jpeg", "image/webp", "image/gif"]
        .into_iter()
        .find(|t| types.contains(t))
        .or_else(|| types.iter().copied().find(|t| t.starts_with("image/")))
}

fn run(cmd: &str, args: &[&str]) -> Option<Vec<u8>> {
    use std::process::{Command, Stdio};
    let mut child = Command::new(cmd)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut out = child.stdout.take()?;
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = std::io::Read::read_to_end(&mut out, &mut buf);
        let _ = tx.send(buf);
    });
    let got = rx.recv_timeout(std::time::Duration::from_secs(2));
    let _ = child.kill();
    let status = child.wait().ok()?;
    let buf = got.ok()?;
    (status.success() || !buf.is_empty())
        .then_some(buf)
        .filter(|b| !b.is_empty())
}

#[cfg(test)]
mod paste_tests {
    #[test]
    fn picks_the_best_image_type() {
        assert_eq!(
            super::image_type("text/plain\nimage/bmp\nimage/png\n"),
            Some("image/png")
        );
        assert_eq!(super::image_type("image/bmp\nTARGETS"), Some("image/bmp"));
        assert_eq!(super::image_type("text/plain\nUTF8_STRING"), None);
    }
}
