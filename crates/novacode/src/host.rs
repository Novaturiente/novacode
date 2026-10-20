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
    pub mcp: Option<Arc<novacode_core::mcp::Hub>>,
    pub extensions: Option<Arc<novacode_core::extensions::Extensions>>,
    pub modes: Option<Arc<novacode_core::modes::Modes>>,
    pub goals: Option<Arc<crate::goals::Goals>>,
}

/// Mode commands (L10.7), handled here so they work in every window and over RPC.
/// Mode commands plus /usage and /debug, all handled in Host::handle.
pub const MODE_COMMANDS: &[(&str, &str)] = &[
    (
        "/plan",
        "plan mode on/off (read-only until a plan is approved); /plan <task> starts planning it",
    ),
    ("/mode", "switch mode preset: /mode <name>, /mode off, /mode lists them"),
    (
        "/goal",
        "work until done: /goal <objective>; /goal shows it; pause, resume, clear, audit on|off",
    ),
    (
        "/usage",
        "tokens and cost per route, cache warming, memory workers, subscription windows",
    ),
    (
        "/debug",
        "route, cache, queues, processes, MCP and log paths, for bug reports",
    ),
];

impl Host {
    /// Do `c` for a window; replies go to `ui`. `local` = the session's own TUI.
    pub async fn handle(self: &Arc<Self>, c: Cmd, ui: &UnboundedSender<UiEvent>, local: bool) {
        let h = &self.hub.handle;
        let steer = matches!(c, Cmd::Steer(_));
        match c {
            Cmd::Prompt(t) | Cmd::Steer(t) if t.starts_with('!') => self.user_shell(&t, ui).await,
            Cmd::Prompt(t) | Cmd::Steer(t) if t.trim() == "/usage" => {
                let (me, ui) = (self.clone(), ui.clone());
                tokio::spawn(async move {
                    let file = session::dir_for(&me.paths, &me.cwd).join(format!("{}.jsonl", me.id));
                    let entries = novacode_core::export::read(&file).map(|(_, e)| e).unwrap_or_default();
                    let cur = me.cur.lock().unwrap().0.clone();
                    let text = format!(
                        "{}\n{}",
                        crate::usage::report(&me.providers, &entries),
                        crate::usage::quotas(me.paths.home(), &me.providers, &cur).await
                    );
                    let _ = ui.send(UiEvent::Page {
                        title: "Usage".into(),
                        text,
                    });
                });
            }
            Cmd::Prompt(t) | Cmd::Steer(t) if t.trim() == "/debug" => {
                let _ = ui.send(UiEvent::Page {
                    title: "Debug".into(),
                    text: self.debug_text(),
                });
            }
            Cmd::Prompt(t) | Cmd::Steer(t) if t.split_whitespace().next() == Some("/goal") => {
                let arg = t.trim().strip_prefix("/goal").unwrap_or("").trim().to_string();
                let Some(g) = &self.goals else {
                    let _ = ui.send(UiEvent::Notice {
                        text: "goals need a window or RPC client (and not a subagent at max depth)".into(),
                        error: true,
                    });
                    return;
                };
                match g.command(&arg) {
                    Ok(text) if arg.is_empty() || arg == "status" => {
                        let _ = ui.send(UiEvent::Page {
                            title: "Goal".into(),
                            text,
                        });
                    }
                    Ok(text) => {
                        let _ = ui.send(UiEvent::Notice { text, error: false });
                    }
                    // Not a subcommand: it's the objective.
                    Err(_) if !matches!(arg.as_str(), "pause" | "resume" | "clear" | "audit on" | "audit off") => {
                        // The how-to rides as a hidden reminder; the transcript shows the goal itself.
                        self.hub.handle.remind_as("goal", crate::goals::draft_prompt(), false);
                        Box::pin(self.handle(Cmd::Prompt(format!("Goal: {arg}")), ui, local)).await;
                    }
                    Err(e) => {
                        let _ = ui.send(UiEvent::Notice { text: e, error: true });
                    }
                }
            }
            Cmd::Prompt(t) | Cmd::Steer(t) if is_mode_command(&t) => {
                if let Some(task) = self.mode_command(&t, ui) {
                    Box::pin(self.handle(Cmd::Prompt(task), ui, local)).await;
                }
            }
            Cmd::Prompt(t) | Cmd::Steer(t) if t.starts_with('/') && self.ext_command(&t, ui).await => {}
            // `/typo` alone on a line is a mistyped command, not a prompt (`/tmp is full` is).
            Cmd::Prompt(t) | Cmd::Steer(t) if self.unknown_command(&t) => {
                let name = t.split_whitespace().next().unwrap_or_default();
                let _ = ui.send(UiEvent::Notice {
                    text: format!("unknown command {name} (Alt+/ lists keys; type / for commands)"),
                    error: true,
                });
            }
            Cmd::Prompt(t) | Cmd::Steer(t) => {
                let t = match &self.extensions {
                    Some(e) => match e.input(&t, steer).await {
                        Some(t) => t,
                        None => return, // an extension handled it
                    },
                    None => t,
                };
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
            Cmd::ExtKey {
                ext,
                tab,
                node,
                key,
                width,
                height,
            } => {
                if let Some(e) = self.extensions.clone() {
                    let payload =
                        serde_json::json!({ "tab": tab, "node": node, "key": key, "width": width, "height": height });
                    tokio::spawn(async move { e.ui_key(&ext, payload).await });
                }
            }
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
            Cmd::Rename(name) => self.hub.handle.rename(name.trim()),
            Cmd::Worktree(arg) => {
                let (h, ui) = (self.hub.handle.clone(), ui.clone());
                // The session's folder: the agent keeps the process cwd in step (move_to).
                let cwd = std::env::current_dir().unwrap_or_else(|_| self.cwd.clone());
                tokio::task::spawn_blocking(move || {
                    use novacode_tools::worktree;
                    let ev = |r: Result<String, String>| match r {
                        Ok(text) => UiEvent::Notice { text, error: false },
                        Err(text) => UiEvent::Notice { text, error: true },
                    };
                    let _ = ui.send(match arg.trim() {
                        "" => match worktree::list(&cwd) {
                            Ok(text) => UiEvent::Page {
                                title: "Worktrees".into(),
                                text,
                            },
                            Err(e) => ev(Err(e)),
                        },
                        a @ ("exit" | "remove") => ev(worktree::here(&cwd).map(|w| {
                            let note = if a == "remove" {
                                worktree::remove(&w)
                            } else {
                                format!(" Worktree kept at {}.", w.top().display())
                            };
                            h.move_to(w.main().to_path_buf());
                            format!("back in the main checkout {}.{note}", w.main().display())
                        })),
                        name => ev(worktree::create(&cwd, name).map(|(path, msg)| {
                            h.move_to(path);
                            msg
                        })),
                    });
                });
            }
            Cmd::Btw(q) => {
                let (h, ui) = (self.hub.handle.clone(), ui.clone());
                let _ = ui.send(UiEvent::Notice {
                    text: format!("btw: asking \"{}\"…", q.lines().next().unwrap_or("")),
                    error: false,
                });
                // Off the command loop: the main thread keeps going meanwhile.
                tokio::spawn(async move {
                    let ev = match h.side_question(&q).await {
                        // Short answers sit in the transcript (never stored); long ones get a page.
                        Ok(a) if a.lines().count() <= 6 && a.len() <= 600 => UiEvent::Notice {
                            text: format!("btw › {a}"),
                            error: false,
                        },
                        Ok(a) => UiEvent::Page {
                            title: format!("btw: {}", q.lines().next().unwrap_or("")),
                            text: a,
                        },
                        Err(e) => UiEvent::Notice {
                            text: format!("btw: {e}"),
                            error: true,
                        },
                    };
                    let _ = ui.send(ev);
                });
            }
            Cmd::Dequeue(steer) => {
                let _ = ui.send(UiEvent::Dequeued {
                    ok: self.hub.handle.dequeue(steer),
                });
            }
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
            Cmd::Mcp => {
                use novacode_core::mcp::Status;
                let text = match &self.mcp {
                    None => "no MCP servers (add one with `novacode mcp add name -- command args…`)".to_string(),
                    Some(h) => h
                        .status()
                        .into_iter()
                        .map(|(name, st, n, d)| {
                            let st = match st {
                                Status::Ready => "ready".to_string(),
                                Status::Connecting => "connecting…".into(),
                                Status::Failed(e) => format!("failed: {e}"),
                            };
                            format!("{name}  {st}  {n} tools\n  {d}\n  log: ~/.novacode/logs/mcp-{name}.log\n")
                        })
                        .collect::<Vec<_>>()
                        .join("\n"),
                };
                let _ = ui.send(UiEvent::Page {
                    title: "MCP servers".into(),
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
            "/mcp" => Cmd::Mcp,
            "/name" if !arg.is_empty() => Cmd::Rename(arg.into()),
            "/worktree" => Cmd::Worktree(arg.into()),
            "/btw" if !arg.is_empty() => Cmd::Btw(arg.into()),
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

    /// `/name args` owned by a script extension: run it and show what it returns.
    pub async fn ext_command(&self, text: &str, ui: &UnboundedSender<UiEvent>) -> bool {
        let Some(exts) = &self.extensions else { return false };
        let Some(r) = exts.command(text).await else {
            return false;
        };
        let (text, error) = match r {
            Ok(t) if t.is_empty() => return true,
            Ok(t) => (t, false),
            Err(e) => (e, true),
        };
        let _ = ui.send(UiEvent::Notice { text, error });
        true
    }

    /// `/plan [task]` and `/mode [name|off]`. Returns a task to send after switching.
    fn mode_command(&self, text: &str, ui: &UnboundedSender<UiEvent>) -> Option<String> {
        let say = |text: String, error: bool| {
            let _ = ui.send(UiEvent::Notice { text, error });
        };
        let Some(m) = &self.modes else {
            say("modes are not available in this session".into(), true);
            return None;
        };
        let (cmd, arg) = text
            .trim()
            .split_once(char::is_whitespace)
            .map_or((text.trim(), ""), |(a, b)| (a, b.trim()));
        let target = match (cmd, arg) {
            ("/plan", "") if m.active().as_deref() == Some("plan") => None,
            ("/plan", _) => Some("plan"),
            ("/mode", "") => {
                say(
                    format!(
                        "modes: {} (active: {})",
                        m.names().join(", "),
                        m.active().unwrap_or_else(|| "none".into())
                    ),
                    false,
                );
                return None;
            }
            ("/mode", "off") => None,
            (_, name) => Some(name),
        };
        match m.set(target) {
            Ok(()) => {
                // A lasting marker in every window's status line while a mode is on.
                self.hub.handle.emit(novacode_types::AgentEvent::Ui {
                    ext: "novacode".into(),
                    op: novacode_types::ui::UiOp::Status {
                        key: "mode".into(),
                        text: target.map(|n| format!("⏸ {n} mode")),
                        style: Some("warn".into()),
                    },
                });
                say(
                    match target {
                        Some(n) => format!("{n} mode on (from the next step)"),
                        None => "mode off".into(),
                    },
                    false,
                )
            }
            Err(e) => {
                say(e, true);
                return None;
            }
        }
        (cmd == "/plan" && !arg.is_empty()).then(|| arg.to_string())
    }

    /// Every slash command: built-ins, then templates and skills (L8.9).
    fn unknown_command(&self, t: &str) -> bool {
        let name = t.split_whitespace().next().unwrap_or_default();
        let shaped = name.len() > 1
            && name[1..]
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "-_:".contains(c))
            && name.as_bytes()[0] == b'/';
        shaped
            && !self
                .commands()
                .iter()
                .any(|(c, _)| c == name || c.split_whitespace().next() == Some(name))
    }

    pub fn commands(&self) -> Vec<(String, String)> {
        let mut v: Vec<(String, String)> = novacode_tui::app::COMMANDS
            .iter()
            .map(|(c, d)| (c.to_string(), d.to_string()))
            .collect();
        v.extend(MODE_COMMANDS.iter().map(|(c, d)| (c.to_string(), d.to_string())));
        v.extend(self.inputs.commands().0);
        v.extend(self.extensions.iter().flat_map(|e| e.commands()));
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

    /// `/debug` (L11.3): what a bug report needs, no secrets.
    fn debug_text(&self) -> String {
        use std::fmt::Write;
        let st = self.hub.state();
        let ctx = self.hub.handle.context();
        let mut s = String::new();
        let _ = writeln!(s, "novacode {}  pid {}", env!("CARGO_PKG_VERSION"), std::process::id());
        let _ = writeln!(s, "session   {}", st.session);
        let _ = writeln!(s, "file      {}", st.file.as_deref().unwrap_or("(none: --no-session)"));
        let _ = writeln!(
            s,
            "cwd       {}  branch {}",
            st.cwd,
            st.branch.as_deref().unwrap_or("-")
        );
        let _ = writeln!(
            s,
            "route     {} · {:?}  window {}",
            st.model, st.level, st.window_tokens
        );
        let _ = writeln!(
            s,
            "activity  {:?} {}  busy {}",
            st.activity.state, st.activity.detail, st.busy
        );
        let _ = writeln!(
            s,
            "context   {} tokens ({:.1}%)",
            st.context_tokens,
            st.context_tokens as f64 * 100.0 / st.window_tokens.max(1) as f64
        );
        let _ = writeln!(
            s,
            "cache     hit {:.0}%  last read {} write {}  last miss {}",
            st.cache.hit_pct,
            st.cache.last_read,
            st.cache.last_write,
            st.cache.last_miss.as_deref().unwrap_or("-")
        );
        let _ = writeln!(s, "warmer    {}", ctx.warm);
        let _ = writeln!(
            s,
            "tokens    in {} out {} cache rd {} wr {}",
            st.tokens.input, st.tokens.output, st.tokens.cache_read, st.tokens.cache_write
        );
        let queued = self.hub.handle.prompts().len();
        let _ = writeln!(s, "prompts   {queued} on the tree");
        match &self.processes {
            Some(p) => {
                let list = p.list();
                let _ = writeln!(s, "processes {} ({} running)", list.len(), p.running());
                for i in list {
                    let _ = writeln!(s, "  {}", serde_json::to_string(&i).unwrap_or_default());
                }
            }
            None => s.push_str("processes off\n"),
        }
        match &self.mcp {
            Some(h) => {
                for (name, st, n, _) in h.status() {
                    let _ = writeln!(s, "mcp       {name} {st:?} {n} tools");
                }
            }
            None => s.push_str("mcp       none\n"),
        }
        let logs = self.paths.logs();
        let _ = writeln!(s, "logs      {}", logs.display());
        let crashes = std::fs::read_dir(&logs)
            .into_iter()
            .flatten()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().starts_with("crash-"))
            .count();
        let _ = writeln!(s, "crashes   {crashes} crash logs");
        s.push_str("\nFor a report: `novacode debug bundle` packs logs and this session, secrets masked.\n");
        s
    }

    pub fn export(&self, html: bool) -> anyhow::Result<PathBuf> {
        let file = session::dir_for(&self.paths, &self.cwd).join(format!("{}.jsonl", self.id));
        export_session(&self.paths, &file, html, None)
    }
}

fn is_mode_command(t: &str) -> bool {
    let first = t.split_whitespace().next().unwrap_or_default();
    matches!(first, "/plan" | "/mode")
}
