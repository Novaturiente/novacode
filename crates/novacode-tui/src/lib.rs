//! Fullscreen terminal UI (L9). Speaks protocol types only: agent and UI
//! events in, commands out.

pub mod app;
pub mod ask;
pub mod dialog;
pub mod editor;
#[cfg(feature = "highlight")]
pub mod highlight;
pub mod images;
pub mod keys;
pub mod markdown;
pub mod panel;
pub mod prompt;
pub mod statusline;
pub mod term;
pub mod theme;
pub mod transcript;
pub mod wrap;

pub use app::{App, Cmd, Exit, Init, UiEvent};

use crossterm::event::EventStream;
use futures_util::StreamExt;
use novacode_types::AgentEvent;
use std::panic::AssertUnwindSafe;
use std::time::{Duration, Instant};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

/// At most 60 frames a second (L9.3).
const FRAME: Duration = Duration::from_millis(16);

/// Run the UI until the user quits or switches session. Input is handled
/// first; the screen redraws only when something changed, never while idle.
pub async fn run(
    init: Init,
    mut events: UnboundedReceiver<AgentEvent>,
    mut ui: UnboundedReceiver<UiEvent>,
    cmds: UnboundedSender<Cmd>,
) -> std::io::Result<Exit> {
    term::install_panic_hook();
    let mut t = term::enter()?;
    let mut app = App::new(init, cmds);
    // L9.17: over SSH or on a loaded machine, fewer frames and a slower spinner.
    let frame = if app.slow() { FRAME * 3 } else { FRAME };
    #[cfg(feature = "images")]
    if let Some(i) = images::Images::detect(app.image_setting()) {
        app.set_images(i);
    }
    let mut input = EventStream::new();
    let mut spin = tokio::time::interval(Duration::from_millis(if app.slow() { 500 } else { 100 }));
    let mut second = tokio::time::interval(Duration::from_secs(1));
    second.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    spin.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut last_draw = Instant::now() - FRAME;
    let mut deadline: Option<tokio::time::Instant> = None;
    let res = loop {
        if app.suspend {
            app.suspend = false;
            if let Err(e) = term::suspend(&mut t) {
                break Err(e);
            }
            app.dirty = true;
        }
        if app.external_editor {
            app.external_editor = false;
            match term::external_editor(&mut t, app.editor.text()) {
                Ok(Some(s)) => app.editor.set_text(&s),
                Ok(None) => {}
                Err(e) => app.tr.notice(format!("$EDITOR failed: {e}"), true),
            }
            app.dirty = true;
        }
        if let Some(provider) = app.login.take() {
            match term::login(&mut t, &provider) {
                // New credentials load on reload; the session carries on.
                Ok(true) => break Ok(Exit::Reload),
                Ok(false) => app.tr.notice(format!("login to {provider} didn't finish"), true),
                Err(e) => app.tr.notice(format!("login failed to start: {e}"), true),
            }
            app.dirty = true;
        }
        if app.dirty {
            if last_draw.elapsed() >= frame {
                // One pacer step per drawn frame (L9.7).
                let behind = app.pace();
                // L9.3: a panic while drawing is reported, not fatal.
                match std::panic::catch_unwind(AssertUnwindSafe(|| t.draw(|f| app.draw(f)).map(|_| ()))) {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => break Err(e),
                    Err(_) => {
                        t = term::enter()?;
                        app.tr.notice("drawing failed; details are in the log", true);
                    }
                }
                last_draw = Instant::now();
                // Text still held back: draw again next frame.
                app.dirty = behind;
                deadline = behind.then(|| tokio::time::Instant::from_std(last_draw + frame));
            } else {
                deadline = Some(tokio::time::Instant::from_std(last_draw + frame));
            }
        }
        if let Some(e) = app.exit.take() {
            break Ok(e);
        }
        tokio::select! {
            biased;
            ev = input.next() => match ev {
                Some(Ok(ev)) => app.on_event(ev),
                Some(Err(e)) => break Err(e),
                None => break Ok(Exit::Quit),
            },
            Some(ev) = ui.recv() => app.on_ui(ev),
            ev = events.recv() => match ev {
                Some(ev) => app.on_agent(ev),
                None => break Ok(Exit::Quit),
            },
            _ = spin.tick(), if app.busy => {
                app.spinner += 1;
                app.dirty = true;
            }
            _ = second.tick(), if app.busy || app.panel.visible => {
                if app.tick() {
                    app.dirty = true;
                }
            }
            _ = tokio::time::sleep_until(deadline.unwrap_or_else(tokio::time::Instant::now)), if deadline.is_some() => {}
        }
    };
    term::restore();
    res
}
