//! The host side of a live session: carries out what any window asks (the TUI's
//! `Cmd`s, wire commands from attached windows and RPC) and answers the window
//! that asked (L8.6, L8.13).

use crate::hub::Hub;
use crate::{Inputs, ago, attach, export_session, models};
use novacode_ai::connect::Providers;
use novacode_ai::trace::Trace;
use novacode_base::paths::Paths;
use novacode_core::session;
use novacode_tui::app::{BranchAction, Cmd, UiEvent};
use novacode_tui::dialog::PickItem;
use novacode_types::ThinkingLevel;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc::UnboundedSender;

pub struct Host {
    pub hub: Arc<Hub>,
    /// The resolved `provider/model` and the level in use, kept for /resume and /new.
    pub cur: Arc<Mutex<(String, ThinkingLevel)>>,
    pub paths: Paths,
    pub cwd: PathBuf,
    pub id: String,
    pub inputs: Inputs,
    /// File rewinds to undo, newest last (L4.15).
    pub undo: Arc<Mutex<Vec<String>>>,
    pub providers: Arc<Providers>,
    pub limits: models::Limits,
    pub trace: Option<Trace>,
    pub processes: Option<Arc<novacode_tools::process::ProcessManager>>,
}

impl Host {
    /// Do `c` for a window; replies go to `ui`. `local` = the session's own TUI.
    pub async fn handle(self: &Arc<Self>, c: Cmd, ui: &UnboundedSender<UiEvent>, local: bool) {
        let h = &self.hub.handle;
        let steer = matches!(c, Cmd::Steer(_));
        match c {
            Cmd::Prompt(t) | Cmd::Steer(t) if t.starts_with('!') => self.user_shell(&t, ui).await,
            Cmd::Prompt(t) | Cmd::Steer(t) => {
                let (blocks, notes) = attach(t, self.cwd.clone(), &self.inputs).await;
                for n in notes {
                    let error = !n.starts_with("attached");
                    let _ = ui.send(UiEvent::Notice { text: n, error });
                }
                if steer {
                    h.steer_content(blocks)
                } else {
                    h.prompt_content(blocks)
                }
            }
            Cmd::Abort => h.abort(),
            Cmd::SetModel(m) => {
                let (me, tx) = (self.clone(), ui.clone());
                tokio::spawn(async move {
                    let level = me.cur.lock().unwrap().1;
                    match models::switch(&me.providers, &m, level, me.limits, me.trace.clone()).await {
                        Ok((sw, route)) => {
                            let price = models::price(&me.providers, &route);
                            let ev = UiEvent::ModelSet {
                                model: route.spec(),
                                window_tokens: sw.window_tokens,
                                level: sw.thinking,
                                price,
                            };
                            *me.cur.lock().unwrap() = (route.spec(), sw.thinking);
                            me.hub.update(local, |s| {
                                s.model = route.spec();
                                s.level = sw.thinking;
                                s.window_tokens = sw.window_tokens;
                                s.price = price;
                            });
                            me.hub.handle.set_model(sw);
                            let _ = tx.send(ev);
                        }
                        Err(e) => {
                            let _ = tx.send(UiEvent::Notice {
                                text: format!("model not changed: {e:#}"),
                                error: true,
                            });
                        }
                    }
                });
            }
            Cmd::SetThinking(l) => {
                let spec = self.cur.lock().unwrap().0.clone();
                let Ok(route) = self.providers.resolve(&spec) else {
                    return;
                };
                let level = route.clamp(l);
                self.cur.lock().unwrap().1 = level;
                h.set_thinking(level);
                self.hub.update(local, |s| s.level = level);
                let mut note = format!("thinking → {} from the next message", level.as_str());
                if level != l {
                    note += &format!(" ({} has no `{}`)", route.model, l.as_str());
                }
                // L1.7: say what the change costs, never block.
                note += if route.thinking_change_keeps_cache() {
                    "; the cache is kept"
                } else {
                    "; the cache starts over"
                };
                let _ = ui.send(UiEvent::Thinking { level, note });
            }
            Cmd::ListModels => {
                let (me, tx) = (self.clone(), ui.clone());
                tokio::spawn(async move {
                    let cur = me.cur.lock().unwrap().0.clone();
                    let (items, errors) = models::list(&me.providers, &cur).await;
                    for e in errors {
                        let _ = tx.send(UiEvent::Notice {
                            text: format!("models: {e}"),
                            error: true,
                        });
                    }
                    let result = if items.is_empty() {
                        Err("no connected provider listed any model".into())
                    } else {
                        Ok(items)
                    };
                    let _ = tx.send(UiEvent::Models { result });
                });
            }
            Cmd::ListSessions => {
                let items = session::list(&self.paths, &self.cwd)
                    .iter()
                    .filter_map(|p| session::summary(p))
                    .filter(|s| s.id != self.id && s.prompts > 0)
                    .map(|s| PickItem {
                        label: if s.title.is_empty() {
                            s.id.clone()
                        } else {
                            s.title.clone()
                        },
                        detail: format!(
                            "{} prompt{} · {}",
                            s.prompts,
                            if s.prompts == 1 { "" } else { "s" },
                            ago(s.modified)
                        ),
                        value: s.path.display().to_string(),
                    })
                    .collect();
                let _ = ui.send(UiEvent::Sessions { items });
            }
            Cmd::Context => {
                let _ = ui.send(UiEvent::Page {
                    title: "Context".into(),
                    text: h.context().render(),
                });
            }
            Cmd::Compact => h.compact(),
            Cmd::Export(html) => {
                let text = match self.export(html) {
                    Ok(p) => format!("exported to {}", p.display()),
                    Err(e) => format!("export failed: {e:#}"),
                };
                let _ = ui.send(UiEvent::Notice { text, error: false });
            }
            Cmd::ListPrompts(action) => {
                let tree = matches!(action, BranchAction::Tree);
                let mut nodes: Vec<_> = h.prompts().into_iter().filter(|p| tree || p.on_branch).collect();
                if !tree {
                    nodes.reverse();
                }
                let mut items: Vec<PickItem> = nodes
                    .into_iter()
                    .map(|p| PickItem {
                        label: if tree {
                            format!(
                                "{}{} {}",
                                "  ".repeat(p.depth),
                                if p.on_branch { "●" } else { "○" },
                                p.text
                            )
                        } else {
                            p.text
                        },
                        detail: novacode_core::memory::utc_minute(p.ts_ms) + "Z",
                        value: p.id,
                    })
                    .collect();
                if matches!(action, BranchAction::Rewind) && !self.undo.lock().unwrap().is_empty() {
                    items.insert(
                        0,
                        PickItem {
                            label: "↩ Undo the last file rewind".into(),
                            detail: String::new(),
                            value: "undo".into(),
                        },
                    );
                }
                let _ = ui.send(UiEvent::Branch { action, items });
            }
            Cmd::Focus(f) => self.hub.set_focus(f),
            Cmd::Background => {
                let n = self.processes.as_ref().map_or(0, |pm| pm.detach_all());
                let text = match n {
                    0 => "no running command to move".to_string(),
                    n => format!(
                        "moved {n} running command{} to the process manager",
                        if n == 1 { "" } else { "s" }
                    ),
                };
                let _ = ui.send(UiEvent::Notice { text, error: false });
            }
            Cmd::Processes => {
                let text = match &self.processes {
                    None => "the process manager is off".to_string(),
                    Some(pm) if pm.list().is_empty() => "no processes".to_string(),
                    Some(pm) => pm
                        .list()
                        .iter()
                        .map(|i| {
                            let state = match &i.end {
                                None => "running".to_string(),
                                Some(e) if e.stopped => "stopped".into(),
                                Some(e) => e.code.map_or("killed".into(), |c| format!("exited {c}")),
                            };
                            let tail = pm.output(&i.id, 15, None).unwrap_or_default();
                            format!("{} {} [{state}]\n$ {}\n{}\n", i.id, i.name, i.command, tail)
                        })
                        .collect::<Vec<_>>()
                        .join("\n"),
                };
                let _ = ui.send(UiEvent::Page {
                    title: "Processes".into(),
                    text,
                });
            }
            Cmd::Quit => {}
        }
    }

    /// A slash command typed over RPC or attach (L8.9): run by the session, never
    /// sent as a prompt. `None` = not one of these (skills and templates expand later).
    pub fn slash(&self, text: &str) -> Option<Result<Cmd, String>> {
        let t = text.trim();
        let (name, arg) = t
            .split_once(char::is_whitespace)
            .map_or((t, ""), |(a, b)| (a, b.trim()));
        Some(Ok(match name {
            "/compact" => Cmd::Compact,
            "/context" => Cmd::Context,
            "/export" => Cmd::Export(arg == "html"),
            "/model" if !arg.is_empty() => Cmd::SetModel(arg.into()),
            "/thinking" if !arg.is_empty() => match ThinkingLevel::parse(arg) {
                Some(l) => Cmd::SetThinking(l),
                None => return Some(Err(format!("unknown thinking level `{arg}`"))),
            },
            n if novacode_tui::app::COMMANDS.iter().any(|(c, _)| *c == n) => {
                return Some(Err(format!("{n} works only in the session's own window")));
            }
            _ => return None,
        }))
    }

    /// Every slash command: built-ins, then templates and skills (L8.9).
    pub fn commands(&self) -> Vec<(String, String)> {
        let mut v: Vec<(String, String)> = novacode_tui::app::COMMANDS
            .iter()
            .map(|(c, d)| (c.to_string(), d.to_string()))
            .collect();
        v.extend(self.inputs.commands().0);
        v
    }

    /// `!cmd` / `!!cmd` (L5.15): the user runs a command. Its output is shown; with
    /// one `!` it also reaches the model at its next call, without starting a turn.
    pub async fn user_shell(&self, text: &str, ui: &UnboundedSender<UiEvent>) {
        let quiet = text.starts_with("!!");
        let cmd = text.trim_start_matches('!').trim().to_string();
        if cmd.is_empty() {
            return;
        }
        let cwd = PathBuf::from(self.hub.state().cwd);
        let run = tokio::process::Command::new("bash")
            .arg("-c")
            .arg(&cmd)
            .current_dir(&cwd)
            .stdin(std::process::Stdio::null())
            .kill_on_drop(true)
            .output();
        let (code, out) = match tokio::time::timeout(std::time::Duration::from_secs(120), run).await {
            Ok(Ok(o)) => {
                let mut s = String::from_utf8_lossy(&o.stdout).into_owned();
                s.push_str(&String::from_utf8_lossy(&o.stderr));
                (o.status.code(), s)
            }
            Ok(Err(e)) => (None, format!("could not run: {e}")),
            Err(_) => (
                None,
                "[killed after 120s; use the process manager for long commands]".into(),
            ),
        };
        let out = out.trim_end();
        // shortcut: the context copy keeps the last 30k chars; the full output isn't kept anywhere.
        let tail: String = {
            let n = out.chars().count();
            out.chars().skip(n.saturating_sub(30_000)).collect()
        };
        let status = match code {
            Some(0) => String::new(),
            Some(c) => format!(" (exit {c})"),
            None => " (killed)".into(),
        };
        let shown: Vec<&str> = out.lines().collect();
        let skip = shown.len().saturating_sub(40);
        let mut note = format!(
            "$ {cmd}{status}{}",
            if quiet { "  · not sent to the model" } else { "" }
        );
        if skip > 0 {
            note += &format!("\n… {skip} lines above");
        }
        for l in &shown[skip..] {
            note += &format!("\n{l}");
        }
        let _ = ui.send(UiEvent::Notice {
            text: note,
            error: code != Some(0),
        });
        if !quiet {
            self.hub.handle.remind(
                format!(
                    "The user ran `{cmd}` in {}{status}. Output:\n```\n{tail}\n```",
                    cwd.display()
                ),
                false,
            );
        }
    }

    pub fn export(&self, html: bool) -> anyhow::Result<PathBuf> {
        let file = session::dir_for(&self.paths, &self.cwd).join(format!("{}.jsonl", self.id));
        export_session(&self.paths, &file, html, None)
    }
}
