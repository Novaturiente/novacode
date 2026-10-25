//! Activity sinks (L8.14, L11.9): tmux pane options in the user's `@agent_*`
//! names, herdr, the `hooks.onActivity` command, and done/blocked notifications.
//! All follow the hub's state; each runs in one task, so updates stay in order.

use crate::hub::Hub;
use crate::wire::{ActState, State};
use novacode_types::AgentEvent;
use std::path::Path;
use std::sync::Arc;
use tokio::process::Command;
use tokio::sync::broadcast;

#[derive(Clone, Default)]
pub struct Sinks {
    /// tmux pane of the session's own window (`$TMUX_PANE`), setting `tmux.agentState`.
    pub tmux_pane: Option<String>,
    /// herdr socket and pane (`HERDR_SOCKET_PATH`, `HERDR_PANE_ID`, `HERDR_ENV=1`).
    pub herdr: Option<(String, String)>,
    /// `hooks.onActivity`: run with `sh -c` on every state change.
    pub hook: Option<String>,
    /// OSC 777 when the session is done or blocked and nobody is looking.
    pub notify: bool,
}

impl Sinks {
    /// For the session's own window. `--bg`, RPC and attach windows have no pane of their own.
    pub fn for_window(settings: &novacode_base::settings::Settings) -> Self {
        let env = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
        Sinks {
            tmux_pane: env("TMUX_PANE").filter(|_| settings.get("tmux.agentState").unwrap_or(true)),
            herdr: (env("HERDR_ENV").as_deref() == Some("1"))
                .then(|| env("HERDR_SOCKET_PATH").zip(env("HERDR_PANE_ID")))
                .flatten(),
            hook: settings.get::<String>("hooks.onActivity"),
            notify: settings.get("notifications.enabled").unwrap_or(true),
        }
    }

    pub fn headless(settings: &novacode_base::settings::Settings) -> Self {
        Sinks {
            hook: settings.get::<String>("hooks.onActivity"),
            ..Default::default()
        }
    }
}

/// Repo name for a folder (main checkout, so worktrees map back); `None` outside git.
pub async fn project(dir: &Path) -> Option<String> {
    let out = Command::from(novacode_base::git::command_in(dir))
        .args(["rev-parse", "--path-format=absolute", "--git-common-dir"])
        .stderr(std::process::Stdio::null())
        .output()
        .await
        .ok()
        .filter(|o| o.status.success())?;
    let common = std::path::PathBuf::from(String::from_utf8_lossy(&out.stdout).trim());
    Some(common.parent()?.file_name()?.to_string_lossy().into_owned())
}

async fn tmux(args: &[&str]) -> Option<String> {
    let out = Command::new("tmux")
        .args(args)
        .stderr(std::process::Stdio::null())
        .output()
        .await
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Is the pane in front of someone: attached session, active window, active pane.
async fn tmux_seen(pane: &str) -> bool {
    tmux(&[
        "display",
        "-p",
        "-t",
        pane,
        "#{&&:#{session_attached},#{&&:#{window_active},#{pane_active}}}",
    ])
    .await
    .as_deref()
        == Some("1")
}

/// OSC 777 to every attached tmux client (reaches the laptop over SSH), or to
/// our own terminal outside tmux.
async fn notify(title: &str, body: &str, in_tmux: bool) {
    let osc = format!("\x1b]777;notify;{title};{body}\x1b\\");
    if in_tmux {
        let clients = tmux(&["list-clients", "-F", "#{client_tty}"]).await.unwrap_or_default();
        for tty in clients.lines().filter(|l| !l.is_empty()) {
            // shortcut: raw write to the client tty, like agent-state.sh; a byte race with tmux redraw is harmless.
            let _ = std::fs::OpenOptions::new()
                .append(true)
                .open(tty)
                .and_then(|mut f| std::io::Write::write_all(&mut f, osc.as_bytes()));
        }
    } else {
        let _ = std::fs::OpenOptions::new()
            .append(true)
            .open("/dev/tty")
            .and_then(|mut f| std::io::Write::write_all(&mut f, osc.as_bytes()));
    }
}

static HERDR_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// One request line to herdr; true once it answers anything.
async fn herdr_send(sock: &str, req: &serde_json::Value) -> bool {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    for wait in [500, 1500] {
        let attempt = async {
            let mut s = tokio::net::UnixStream::connect(sock).await.ok()?;
            s.write_all(format!("{req}\n").as_bytes()).await.ok()?;
            let mut b = [0u8; 1];
            (s.read(&mut b).await.ok()? > 0).then_some(())
        };
        if let Ok(Some(())) = tokio::time::timeout(std::time::Duration::from_millis(wait), attempt).await {
            return true;
        }
    }
    false
}

async fn herdr(sock: &str, pane: &str, method: &str, mut params: serde_json::Value, s: &State) {
    let seq = HERDR_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + novacode_core::session::now_ms() * 1000;
    params["pane_id"] = pane.into();
    params["source"] = "herdr:novacode".into();
    params["agent"] = "novacode".into();
    params["seq"] = seq.into();
    match &s.file {
        Some(f) => params["agent_session_path"] = f.as_str().into(),
        None => params["agent_session_id"] = s.session.as_str().into(),
    }
    let req = serde_json::json!({ "id": format!("herdr:novacode:{seq}"), "method": method, "params": params });
    if !herdr_send(sock, &req).await {
        tracing::debug!(method, "herdr did not answer");
    }
}

async fn run_hook(cmd: &str, s: &State, prev: ActState) {
    use tokio::io::AsyncWriteExt;
    let child = Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .env("NOVACODE_SESSION", &s.session)
        .env("NOVACODE_SESSION_FILE", s.file.as_deref().unwrap_or(""))
        .env("NOVACODE_STATE", s.activity.state.as_str())
        .env("NOVACODE_PREV_STATE", prev.as_str())
        .env("NOVACODE_DETAIL", &s.activity.detail)
        .env("NOVACODE_CWD", &s.cwd)
        .env("NOVACODE_MODEL", &s.model)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn();
    let Ok(mut child) = child else { return };
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin
            .write_all(serde_json::to_string(s).unwrap_or_default().as_bytes())
            .await;
    }
    if tokio::time::timeout(std::time::Duration::from_secs(10), child.wait())
        .await
        .is_err()
    {
        tracing::warn!(cmd, "hooks.onActivity took over 10s; killed");
    }
}

/// Follow the hub until it closes; then clear the tmux options (agent "off").
pub fn spawn(hub: &Arc<Hub>, sinks: Sinks) -> tokio::task::JoinHandle<()> {
    let mut changed = hub.changed.subscribe();
    let mut events = hub.handle.subscribe();
    let hub = hub.clone();
    tokio::spawn(async move {
        let mut last: Option<State> = None;
        let mut edited_project: Option<String> = None;
        let cwd_project = project(Path::new(&hub.state().cwd)).await;
        if let Some((sock, pane)) = &sinks.herdr {
            herdr(
                sock,
                pane,
                "pane.report_agent_session",
                serde_json::json!({}),
                &hub.state(),
            )
            .await;
        }
        loop {
            if hub.is_closed() {
                break;
            }
            let s = changed.borrow_and_update().clone();
            let prev = last.as_ref().map_or(ActState::Idle, |l| l.activity.state);
            let state_changed = last.as_ref().is_none_or(|l| l.activity.state != s.activity.state);
            let route_changed = last.as_ref().is_none_or(|l| l.model != s.model);
            if let Some(p) = &sinks.tmux_pane
                && last.as_ref().is_none_or(|l| l.processes.len() != s.processes.len())
            {
                // Apart from the state, so a long-lived dev server never blocks the done alert.
                let n = s.processes.len().to_string();
                if s.processes.is_empty() {
                    tmux(&["set", "-p", "-t", p, "-u", "@agent_bg"]).await;
                } else {
                    tmux(&["set", "-p", "-t", p, "@agent_bg", &n]).await;
                }
            }
            if let Some(pane) = &sinks.tmux_pane
                && (state_changed || route_changed)
            {
                let p = pane.as_str();
                // agent-state.sh: idle on an unwatched pane after work = done; the
                // user's pane-focus-in hook turns done back to idle.
                let seen = tmux_seen(p).await;
                let mut st = s.activity.state.as_str();
                let shown = tmux(&["show", "-p", "-t", p, "-qv", "@agent_state"])
                    .await
                    .unwrap_or_default();
                if st == "idle" && !seen && matches!(shown.as_str(), "working" | "blocked" | "done") {
                    st = "done";
                }
                let proj = edited_project.clone().or_else(|| cwd_project.clone());
                let file = s.file.clone().unwrap_or_default();
                let mut args = vec![
                    "set",
                    "-p",
                    "-t",
                    p,
                    "@agent",
                    "novacode",
                    ";",
                    "set",
                    "-p",
                    "-t",
                    p,
                    "@agent_state",
                    st,
                    ";",
                    "set",
                    "-p",
                    "-t",
                    p,
                    "@agent_cwd",
                    &s.cwd,
                    ";",
                    "set",
                    "-p",
                    "-t",
                    p,
                    "@agent_model",
                    &s.model,
                ];
                if !file.is_empty() {
                    args.extend([";", "set", "-p", "-t", p, "@agent_session", &file]);
                }
                if let Some(pr) = &proj {
                    args.extend([";", "set", "-p", "-t", p, "@agent_project", pr]);
                }
                tmux(&args).await;
                if state_changed && sinks.notify && !seen && matches!(st, "done" | "blocked") {
                    let place = Path::new(&s.cwd)
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    notify(&format!("novacode {st}"), &place, true).await;
                }
            } else if state_changed && sinks.notify && sinks.tmux_pane.is_none() && !hub.focused() {
                let done = prev != ActState::Idle && s.activity.state == ActState::Idle;
                if done || s.activity.state == ActState::Blocked {
                    let place = Path::new(&s.cwd)
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    let title = if done { "novacode done" } else { "novacode needs you" };
                    notify(title, &place, false).await;
                }
            }
            if state_changed {
                if let Some((sock, pane)) = &sinks.herdr {
                    let msg = (s.activity.state == ActState::Blocked).then(|| s.activity.detail.clone());
                    herdr(
                        sock,
                        pane,
                        "pane.report_agent",
                        serde_json::json!({ "state": s.activity.state.as_str(), "message": msg }),
                        &s,
                    )
                    .await;
                }
                if let Some(cmd) = &sinks.hook {
                    run_hook(cmd, &s, prev).await;
                }
            }
            last = Some(s);
            tokio::select! {
                r = changed.changed() => if r.is_err() { break },
                ev = events.recv() => match ev {
                    // The edited file's repo, so the sidebar shows where the work happens.
                    Ok(AgentEvent::ToolStart { call }) if matches!(call.name.as_str(), "edit" | "write" | "apply_patch") => {
                        if let Some(path) = call.arguments["path"].as_str() {
                            let abs = Path::new(&hub.state().cwd).join(path);
                            if let Some(pr) = project(abs.parent().unwrap_or(&abs)).await
                                && edited_project.as_ref() != Some(&pr)
                            {
                                edited_project = Some(pr.clone());
                                if let Some(p) = &sinks.tmux_pane {
                                    tmux(&["set", "-p", "-t", p, "@agent_project", &pr]).await;
                                }
                            }
                        }
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                    _ => {}
                },
            }
        }
        if let Some(p) = &sinks.tmux_pane {
            let mut args = vec![];
            for o in [
                "@agent",
                "@agent_state",
                "@agent_session",
                "@agent_cwd",
                "@agent_project",
                "@agent_model",
                "@agent_bg",
            ] {
                if !args.is_empty() {
                    args.push(";");
                }
                args.extend(["set", "-p", "-t", p.as_str(), "-u", o]);
            }
            tmux(&args).await;
        }
    })
}
