//! Fullscreen terminal UI (L9). Speaks protocol types only: agent events in,
//! commands out.

pub mod app;
pub mod editor;
pub mod markdown;
pub mod term;
pub mod theme;
pub mod transcript;
pub mod wrap;

pub use app::{App, Cmd, Init};

use crossterm::event::EventStream;
use futures_util::StreamExt;
use novacode_types::AgentEvent;
use std::panic::AssertUnwindSafe;
use std::time::{Duration, Instant};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

/// At most 60 frames a second (L9.3).
const FRAME: Duration = Duration::from_millis(16);

/// Run the UI until the user quits. Input is handled first; the screen
/// redraws only when something changed, and never while idle.
pub async fn run(
    init: Init,
    mut events: UnboundedReceiver<AgentEvent>,
    cmds: UnboundedSender<Cmd>,
) -> std::io::Result<()> {
    term::install_panic_hook();
    let mut t = term::enter()?;
    let mut app = App::new(init, cmds.clone());
    let mut input = EventStream::new();
    let mut spin = tokio::time::interval(Duration::from_millis(100));
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
        if app.clear {
            app.clear = false;
            let _ = t.clear();
            app.dirty = true;
        }
        if app.dirty {
            if last_draw.elapsed() >= FRAME {
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
                app.dirty = false;
                deadline = None;
            } else {
                deadline = Some(tokio::time::Instant::from_std(last_draw + FRAME));
            }
        }
        if app.quit {
            break Ok(());
        }
        tokio::select! {
            biased;
            ev = input.next() => match ev {
                Some(Ok(ev)) => app.on_event(ev),
                Some(Err(e)) => break Err(e),
                None => break Ok(()),
            },
            ev = events.recv() => match ev {
                Some(ev) => app.on_agent(ev),
                None => break Ok(()),
            },
            _ = spin.tick(), if app.busy => {
                app.spinner += 1;
                app.dirty = true;
            }
            _ = tokio::time::sleep_until(deadline.unwrap_or_else(tokio::time::Instant::now)), if deadline.is_some() => {}
        }
    };
    term::restore();
    let _ = cmds.send(Cmd::Quit);
    res
}
