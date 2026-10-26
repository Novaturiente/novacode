//! Serving a live session over the wire protocol (L8.6, L8.13): the per-session
//! socket, RPC on stdin/stdout, the run file, `attach`, `agents` and `stop`.

use crate::host::Host;
use crate::wire::{ClientCmd, PROTOCOL, Request, ServerMsg, State, line};
use novacode_base::paths::Paths;
use novacode_tui::app::{Cmd, UiEvent};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;

/// `bye` reason when the owner window rebuilds the same session (rewind, tree,
/// reload, restart): attached windows wait for it and reconnect.
pub const RELOADING: &str = "reloading the session";

pub fn run_dir(paths: &Paths) -> PathBuf {
    paths.home().join("run")
}

/// What `agents` and the status sinks read: `<home>/run/<id>.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunFile {
    pub pid: u32,
    pub socket: PathBuf,
    /// Started with `--bg`: outlives its terminal.
    pub bg: bool,
    /// tmux pane of the session's own window, so `status` in that pane finds it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pane: Option<String>,
    pub started_ms: u64,
    pub state: State,
}

/// Serve one client until it leaves. With `eof_ends_session` (RPC), the end of
/// input ends the session and the output runs on until the session says bye.
pub async fn serve<R, W>(r: R, mut w: W, host: Arc<Host>, eof_ends_session: bool)
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let hub = host.hub.clone();
    let (cid, first, tx, mut rx) = hub.connect();
    let writer = tokio::spawn(async move {
        for m in first {
            let bye = matches!(m, ServerMsg::Bye { .. });
            if w.write_all(line(&m).as_bytes()).await.is_err() || bye {
                let _ = w.flush().await;
                return;
            }
        }
        let _ = w.flush().await;
        while let Some(m) = rx.recv().await {
            let bye = matches!(m, ServerMsg::Bye { .. });
            if w.write_all(line(&m).as_bytes()).await.is_err() || w.flush().await.is_err() || bye {
                break;
            }
        }
    });
    // Replies the host sends to this window only.
    let (ui_tx, mut ui_rx) = mpsc::unbounded_channel::<UiEvent>();
    let to_client = tx.clone();
    tokio::spawn(async move {
        while let Some(ui) = ui_rx.recv().await {
            let Ok(ui) = serde_json::to_value(&ui) else { continue };
            if to_client.send(ServerMsg::Ui { ui }).is_err() {
                break;
            }
        }
    });
    let link = Arc::new(ClientLink {
        tx: tx.clone(),
        pending: Default::default(),
    });
    let mut lines = BufReader::new(r).lines();
    while let Ok(Some(l)) = lines.next_line().await {
        if l.trim().is_empty() {
            continue;
        }
        let req: Request = match serde_json::from_str(&l) {
            Ok(r) => r,
            Err(e) => {
                let _ = tx.send(ServerMsg::Response {
                    id: None,
                    command: "parse".into(),
                    ok: false,
                    data: Value::Null,
                    error: Some(format!("bad command: {e}")),
                });
                continue;
            }
        };
        let command = req.cmd.name().to_string();
        let shutdown = matches!(req.cmd, ClientCmd::Shutdown);
        let res = match req.cmd {
            ClientCmd::RegisterTools { tools } => {
                let n = tools.len();
                let tools: Vec<Arc<dyn novacode_tools::Tool>> = tools
                    .into_iter()
                    .map(|def| {
                        Arc::new(ClientTool {
                            def,
                            link: link.clone(),
                        }) as Arc<dyn novacode_tools::Tool>
                    })
                    .collect();
                hub.handle.add_tools(tools);
                Ok(json!({ "registered": n }))
            }
            ClientCmd::ToolResult {
                call,
                content,
                is_error,
            } => match link.pending.lock().unwrap().remove(&call) {
                Some(done) => {
                    let _ = done.send(if is_error {
                        novacode_tools::ToolOutput::error(content)
                    } else {
                        novacode_tools::ToolOutput::text(content)
                    });
                    Ok(Value::Null)
                }
                None => Err(format!("no tool call {call} is waiting")),
            },
            cmd => dispatch(&host, cmd, &ui_tx).await,
        };
        if req.id.is_some() || res.is_err() {
            let (ok, data, error) = match res {
                Ok(d) => (true, d, None),
                Err(e) => (false, Value::Null, Some(e)),
            };
            let _ = tx.send(ServerMsg::Response {
                id: req.id,
                command,
                ok,
                data,
                error,
            });
        }
        if shutdown {
            break;
        }
    }
    // Calls waiting on this client fail now (their senders drop).
    link.pending.lock().unwrap().clear();
    if eof_ends_session {
        hub.handle.shutdown();
    } else {
        hub.disconnect(cid);
    }
    drop((tx, ui_tx));
    let _ = writer.await;
}

/// One connected client that runs tools for the session (L7.8).
struct ClientLink {
    tx: mpsc::UnboundedSender<ServerMsg>,
    pending: std::sync::Mutex<std::collections::HashMap<u64, tokio::sync::oneshot::Sender<novacode_tools::ToolOutput>>>,
}

static CALL_IDS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

struct ClientTool {
    def: crate::wire::ClientToolDef,
    link: Arc<ClientLink>,
}

impl novacode_tools::Tool for ClientTool {
    fn name(&self) -> &str {
        &self.def.name
    }
    fn description(&self) -> &str {
        &self.def.description
    }
    fn schema(&self) -> Value {
        if self.def.parameters.is_object() {
            self.def.parameters.clone()
        } else {
            json!({ "type": "object", "properties": {} })
        }
    }
    fn execute<'a>(
        &'a self,
        args: Value,
        ctx: &'a novacode_tools::ToolCtx,
    ) -> futures_util::future::BoxFuture<'a, novacode_tools::ToolOutput> {
        Box::pin(async move {
            let id = CALL_IDS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let (done, rx) = tokio::sync::oneshot::channel();
            self.link.pending.lock().unwrap().insert(id, done);
            let msg = ServerMsg::ToolCall {
                id,
                name: self.def.name.clone(),
                args,
            };
            if self.link.tx.send(msg).is_err() {
                return novacode_tools::ToolOutput::error("the client that runs this tool has disconnected");
            }
            tokio::select! {
                r = rx => r.unwrap_or_else(|_| novacode_tools::ToolOutput::error("the client that runs this tool has disconnected")),
                _ = ctx.cancel.cancelled() => {
                    self.link.pending.lock().unwrap().remove(&id);
                    novacode_tools::ToolOutput::error("cancelled")
                }
            }
        })
    }
}

async fn dispatch(host: &Arc<Host>, cmd: ClientCmd, ui: &mpsc::UnboundedSender<UiEvent>) -> Result<Value, String> {
    let hub = &host.hub;
    let via = |c: Cmd| async move {
        host.handle(c, ui, false).await;
        Ok(Value::Null)
    };
    match cmd {
        ClientCmd::Prompt { text } if text.starts_with('!') => {
            host.user_shell(&text, ui).await;
            Ok(json!({ "disposition": "handled" }))
        }
        ClientCmd::Prompt { text } if text.starts_with('/') && host.slash(&text).is_some() => {
            let cmd = host.slash(&text).unwrap()?;
            host.handle(cmd, ui, false).await;
            Ok(json!({ "disposition": "handled" }))
        }
        ClientCmd::GetCommands => Ok(json!({
            "commands": host.commands().into_iter().map(|(name, description)| json!({"name": name, "description": description})).collect::<Vec<_>>()
        })),
        ClientCmd::Prompt { text } => {
            let disposition = if hub.state().busy { "queued" } else { "started" };
            // Busy from hand-in, not from agent_start: a second prompt right after is queued.
            hub.update(false, |s| s.busy = true);
            host.handle(Cmd::Prompt(text), ui, false).await;
            Ok(json!({ "disposition": disposition }))
        }
        ClientCmd::Steer { text } => via(Cmd::Steer(text)).await,
        ClientCmd::Abort => via(Cmd::Abort).await,
        ClientCmd::SetModel { model } => via(Cmd::SetModel(model)).await,
        ClientCmd::SetThinking { level } => via(Cmd::SetThinking(level)).await,
        ClientCmd::Compact => via(Cmd::Compact).await,
        ClientCmd::ListModels => via(Cmd::ListModels).await,
        ClientCmd::GetContext => Ok(json!({ "text": hub.handle.context().render() })),
        ClientCmd::Export { html } => host
            .export(html)
            .map(|p| json!({ "path": p }))
            .map_err(|e| format!("{e:#}")),
        ClientCmd::GetState => Ok(serde_json::to_value(hub.state()).unwrap_or_default()),
        ClientCmd::GetMessages => Ok(json!({ "messages": hub.history() })),
        ClientCmd::Approval { approval, answer } => {
            hub.answer(approval, answer);
            Ok(Value::Null)
        }
        ClientCmd::Answer { ask, replies } => {
            hub.answer_ask(ask, replies);
            Ok(Value::Null)
        }
        ClientCmd::Shutdown => {
            hub.handle.abort();
            hub.handle.shutdown();
            Ok(Value::Null)
        }
        ClientCmd::ExtKey {
            ext,
            tab,
            node,
            key,
            width,
            height,
        } => {
            if let Some(e) = host.extensions.clone() {
                tokio::spawn(async move {
                    e.ui_key(
                        &ext,
                        json!({ "tab": tab, "node": node, "key": key, "width": width, "height": height }),
                    )
                    .await
                });
            }
            Ok(Value::Null)
        }
        // Handled in `serve`, which owns the client link.
        ClientCmd::RegisterTools { .. } | ClientCmd::ToolResult { .. } => Err("not available here".into()),
    }
}

/// While a session is live: its socket and run file. `end()` removes both.
pub struct Live {
    sock: PathBuf,
    run: PathBuf,
    tasks: Vec<tokio::task::JoinHandle<()>>,
    pub host: Arc<Host>,
}

impl Live {
    pub fn add_task(&mut self, t: tokio::task::JoinHandle<()>) {
        self.tasks.push(t);
    }

    /// Close the hub, then give the sinks a moment to clear tmux options.
    pub async fn end(mut self, reason: &str) {
        self.host.hub.close(reason);
        self.host.hub.changed.send_modify(|_| {});
        let rest = self.tasks.split_off(2.min(self.tasks.len()));
        for t in &self.tasks {
            t.abort();
        }
        for t in rest {
            let _ = tokio::time::timeout(std::time::Duration::from_secs(2), t).await;
        }
        let _ = std::fs::remove_file(&self.sock);
        let _ = std::fs::remove_file(&self.run);
        let _ = std::fs::remove_file(novacode_base::storage::backup_path(&self.run));
    }
}

/// A live session's address: `<run>/<id>.sock`, or a named pipe on Windows.
fn address(dir: &Path, id: &str) -> PathBuf {
    if cfg!(windows) {
        format!(r"\\.\pipe\novacode-{id}").into()
    } else {
        dir.join(format!("{id}.sock"))
    }
}

type Reader = Box<dyn AsyncRead + Unpin + Send>;
type Writer = Box<dyn AsyncWrite + Unpin + Send>;

/// Accept clients on `sock` (owner only), each handed to `on`.
#[cfg(unix)]
fn listen(sock: &Path, on: impl Fn(Reader, Writer) + Send + 'static) -> std::io::Result<tokio::task::JoinHandle<()>> {
    let _ = std::fs::remove_file(sock);
    let listener = tokio::net::UnixListener::bind(sock)?;
    novacode_base::platform::set_mode(sock, 0o600)?;
    Ok(tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let (r, w) = stream.into_split();
            on(Box::new(r), Box::new(w));
        }
    }))
}

/// One pipe instance per client; a new one waits before the last is handed off.
/// `first_pipe_instance`: fails if another process already owns the name.
// shortcut: the pipe's default DACL lets other local users open it read-only; set an owner-only DACL for shared machines.
#[cfg(windows)]
fn listen(sock: &Path, on: impl Fn(Reader, Writer) + Send + 'static) -> std::io::Result<tokio::task::JoinHandle<()>> {
    use tokio::net::windows::named_pipe::ServerOptions;
    let name = sock.as_os_str().to_owned();
    let mut server = ServerOptions::new().first_pipe_instance(true).create(&name)?;
    Ok(tokio::spawn(async move {
        while server.connect().await.is_ok() {
            let Ok(next) = ServerOptions::new().create(&name) else {
                break;
            };
            let (r, w) = tokio::io::split(std::mem::replace(&mut server, next));
            on(Box::new(r), Box::new(w));
        }
    }))
}

/// Connect to a live session's address.
#[cfg(unix)]
async fn connect(sock: &Path) -> std::io::Result<tokio::net::UnixStream> {
    tokio::net::UnixStream::connect(sock).await
}

/// Busy = every instance taken for a moment; the server makes the next one at once.
#[cfg(windows)]
async fn connect(sock: &Path) -> std::io::Result<tokio::net::windows::named_pipe::NamedPipeClient> {
    const ERROR_PIPE_BUSY: i32 = 231;
    let mut tries = 0;
    loop {
        match tokio::net::windows::named_pipe::ClientOptions::new().open(sock) {
            Err(e) if e.raw_os_error() == Some(ERROR_PIPE_BUSY) && tries < 40 => {
                tries += 1;
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
            r => return r,
        }
    }
}

/// Listen on `<home>/run/<id>.sock` (owner only) and keep the run file current.
pub fn go_live(host: &Arc<Host>, bg: bool) -> std::io::Result<Live> {
    let dir = run_dir(&host.paths);
    std::fs::create_dir_all(&dir)?;
    novacode_base::platform::set_mode(&dir, 0o700)?;
    let sock = address(&dir, &host.id);
    let run = dir.join(format!("{}.json", host.id));
    let h = host.clone();
    let accept = listen(&sock, move |r, w| {
        tokio::spawn(serve(r, w, h.clone(), false));
    })?;
    let mut file = RunFile {
        pid: std::process::id(),
        socket: sock.clone(),
        bg,
        pane: if bg { None } else { std::env::var("TMUX_PANE").ok() },
        started_ms: novacode_core::session::now_ms(),
        state: host.hub.state(),
    };
    let mut changed = host.hub.changed.subscribe();
    let path = run.clone();
    let writer = tokio::spawn(async move {
        loop {
            let _ = novacode_base::storage::write_json(&path, &file, novacode_base::storage::Durability::Fast);
            if changed.changed().await.is_err() {
                break;
            }
            file.state = changed.borrow_and_update().clone();
        }
    });
    Ok(Live {
        sock,
        run,
        tasks: vec![accept, writer],
        host: host.clone(),
    })
}

/// Live sessions, newest first. Files of dead processes are removed.
pub fn live_sessions(paths: &Paths) -> Vec<RunFile> {
    let mut v: Vec<RunFile> = std::fs::read_dir(run_dir(paths))
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
        .filter_map(|e| {
            let f: RunFile = novacode_base::storage::read_json(&e.path()).ok().flatten()?;
            if novacode_tools::process::pid_alive(f.pid) {
                Some(f)
            } else {
                let _ = std::fs::remove_file(e.path());
                let _ = std::fs::remove_file(novacode_base::storage::backup_path(&e.path()));
                let _ = std::fs::remove_file(&f.socket);
                None
            }
        })
        .collect();
    v.sort_by_key(|f| std::cmp::Reverse(f.started_ms));
    v
}

pub fn find_live(paths: &Paths, prefix: &str) -> anyhow::Result<RunFile> {
    let all = live_sessions(paths);
    let hits: Vec<_> = all
        .into_iter()
        .filter(|f| f.state.session.starts_with(prefix))
        .collect();
    match hits.len() {
        0 => anyhow::bail!("no live session starts with {prefix} (see `novacode agents`)"),
        1 => Ok(hits.into_iter().next().unwrap()),
        n => anyhow::bail!("{n} live sessions start with {prefix}; give more of the id"),
    }
}

/// `novacode agents`: live sessions on this machine.
pub fn agents(paths: &Paths) -> std::process::ExitCode {
    let all = live_sessions(paths);
    if all.is_empty() {
        eprintln!("no live sessions");
        return std::process::ExitCode::SUCCESS;
    }
    println!("{:<15} {:<8} {:<4} {:<34} cwd", "session", "state", "bg", "model");
    for f in all {
        let s = &f.state;
        println!(
            "{:<15} {:<8} {:<4} {:<34} {}",
            &s.session[..s.session.len().min(13)],
            match s.activity.state {
                crate::wire::ActState::Blocked => "blocked",
                _ if s.busy => "working",
                _ => "idle",
            },
            if f.bg { "yes" } else { "" },
            format!("{}:{}", s.model, s.level.as_str()),
            s.cwd
        );
    }
    std::process::ExitCode::SUCCESS
}

/// `novacode stop <id>`: end a live session (a `--bg` one included).
pub async fn stop(paths: &Paths, prefix: &str) -> anyhow::Result<std::process::ExitCode> {
    let f = find_live(paths, prefix)?;
    let mut s = connect(&f.socket).await?;
    s.write_all(
        line(&Request {
            id: None,
            cmd: ClientCmd::Shutdown,
        })
        .as_bytes(),
    )
    .await?;
    s.flush().await?;
    eprintln!("stopping {}", f.state.session);
    Ok(std::process::ExitCode::SUCCESS)
}

/// `novacode attach <id>`: another window on a live session. Leaving only detaches.
pub async fn attach(
    paths: &Paths,
    prefix: &str,
    base: crate::TuiBase,
    panel_only: bool,
) -> anyhow::Result<Option<std::process::ExitCode>> {
    use anyhow::Context;
    let f = find_live(paths, prefix)?;
    let stream = connect(&f.socket)
        .await
        .with_context(|| format!("connecting to {}", f.socket.display()))?;
    let (r, mut w) = tokio::io::split(stream);
    let mut lines = BufReader::new(r).lines();
    let mut next = async || -> anyhow::Result<ServerMsg> {
        let l = lines.next_line().await?.context("the session closed the connection")?;
        Ok(serde_json::from_str(&l)?)
    };
    let ServerMsg::Hello { protocol, state, .. } = next().await? else {
        anyhow::bail!("not a novacode session socket")
    };
    if protocol != PROTOCOL {
        anyhow::bail!("that session speaks protocol {protocol}, this novacode {PROTOCOL}: use the same version");
    }
    let ServerMsg::History { messages } = next().await? else {
        anyhow::bail!("protocol error: no history")
    };
    let id = state.session.clone();
    let (etx, erx) = mpsc::unbounded_channel();
    let (ui_tx, ui_rx) = mpsc::unbounded_channel();
    let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<Cmd>();
    let (req_tx, mut req_rx) = mpsc::unbounded_channel::<Request>();
    tokio::spawn(async move {
        while let Some(r) = req_rx.recv().await {
            if w.write_all(line(&r).as_bytes()).await.is_err() || w.flush().await.is_err() {
                break;
            }
        }
    });
    let approvals = req_tx.clone();
    let mut last = state.clone();
    let bye: Arc<std::sync::Mutex<Option<String>>> = Arc::default();
    let bye_seen = bye.clone();
    tokio::spawn(async move {
        while let Ok(Some(l)) = lines.next_line().await {
            let Ok(m) = serde_json::from_str::<ServerMsg>(&l) else {
                continue;
            };
            match m {
                ServerMsg::Event { event } => {
                    let _ = etx.send(event);
                }
                ServerMsg::Ui { ui } => {
                    if let Ok(ui) = serde_json::from_value::<UiEvent>(ui) {
                        let _ = ui_tx.send(ui);
                    }
                }
                ServerMsg::Approve {
                    id,
                    tool,
                    detail,
                    reason,
                    rule,
                    risky,
                } => {
                    let (tx, rx) = tokio::sync::oneshot::channel();
                    let _ = ui_tx.send(UiEvent::Approve(novacode_tui::dialog::ApproveReq {
                        id,
                        tool,
                        detail,
                        reason,
                        rule,
                        risky,
                        reply: tx,
                    }));
                    let out = approvals.clone();
                    tokio::spawn(async move {
                        if let Ok(answer) = rx.await {
                            let _ = out.send(Request {
                                id: None,
                                cmd: ClientCmd::Approval { approval: id, answer },
                            });
                        }
                    });
                }
                ServerMsg::ApprovalDone { id } => {
                    let _ = ui_tx.send(UiEvent::ApprovalDone { id });
                }
                ServerMsg::Ask { id, questions } => {
                    let (tx, rx) = tokio::sync::oneshot::channel();
                    let _ = ui_tx.send(UiEvent::Ask(novacode_tui::ask::AskReq {
                        id,
                        questions,
                        reply: tx,
                    }));
                    let out = approvals.clone();
                    tokio::spawn(async move {
                        if let Ok(replies) = rx.await {
                            let _ = out.send(Request {
                                id: None,
                                cmd: ClientCmd::Answer { ask: id, replies },
                            });
                        }
                    });
                }
                ServerMsg::AskDone { id } => {
                    let _ = ui_tx.send(UiEvent::AskDone { id });
                }
                ServerMsg::State { state } => {
                    if state.info_key() != last.info_key() {
                        let _ = ui_tx.send(state.info());
                    }
                    if state.model != last.model || state.level != last.level {
                        let _ = ui_tx.send(UiEvent::ModelSet {
                            model: state.model.clone(),
                            window_tokens: state.window_tokens,
                            level: state.level,
                            price: state.price,
                        });
                    }
                    last = state;
                }
                ServerMsg::Response {
                    command,
                    ok,
                    data,
                    error,
                    ..
                } => {
                    let ui = match (command.as_str(), ok) {
                        (_, false) => UiEvent::Notice {
                            text: format!("{command}: {}", error.unwrap_or_default()),
                            error: true,
                        },
                        ("get_context", true) => UiEvent::Page {
                            title: "Context".into(),
                            text: data["text"].as_str().unwrap_or_default().into(),
                        },
                        ("export", true) => UiEvent::Notice {
                            text: format!("exported to {}", data["path"].as_str().unwrap_or_default()),
                            error: false,
                        },
                        _ => continue,
                    };
                    let _ = ui_tx.send(ui);
                }
                // The window closes with the session; `etx` drops here.
                ServerMsg::Bye { reason } => {
                    *bye_seen.lock().unwrap() = Some(reason);
                    break;
                }
                ServerMsg::Hello { .. } | ServerMsg::History { .. } | ServerMsg::ToolCall { .. } => {}
            }
        }
    });
    tokio::spawn(async move {
        while let Some(c) = cmd_rx.recv().await {
            let cmd = match c {
                Cmd::Prompt(text) => ClientCmd::Prompt { text },
                Cmd::Steer(text) => ClientCmd::Steer { text },
                Cmd::Abort => ClientCmd::Abort,
                Cmd::SetModel(model) => ClientCmd::SetModel { model },
                Cmd::SetThinking(level) => ClientCmd::SetThinking { level },
                Cmd::ListModels => ClientCmd::ListModels,
                Cmd::Context => ClientCmd::GetContext,
                Cmd::Compact => ClientCmd::Compact,
                Cmd::Export(html) => ClientCmd::Export { html },
                Cmd::Mcp => ClientCmd::Prompt { text: "/mcp".into() },
                Cmd::Docs => ClientCmd::Prompt { text: "/docs".into() },
                Cmd::Btw(q) => ClientCmd::Prompt {
                    text: format!("/btw {q}"),
                },
                Cmd::Rename(name) => ClientCmd::Prompt {
                    text: format!("/name {name}"),
                },
                Cmd::Worktree(arg) => ClientCmd::Prompt {
                    text: format!("/worktree {arg}"),
                },
                Cmd::ExtKey {
                    ext,
                    tab,
                    node,
                    key,
                    width,
                    height,
                } => ClientCmd::ExtKey {
                    ext,
                    tab,
                    node,
                    key,
                    width,
                    height,
                },
                // The session-swapping commands are refused by the attached TUI itself.
                Cmd::ListSessions
                | Cmd::ListPrompts(_)
                | Cmd::Focus(_)
                | Cmd::Background
                | Cmd::Processes
                | Cmd::Dequeue(_)
                | Cmd::TakeFollowUps
                | Cmd::LoginList
                | Cmd::Login { .. }
                | Cmd::LoginInput(_)
                | Cmd::LoginCancel
                | Cmd::FileStats(_)
                | Cmd::OpenFile(_) => continue,
                Cmd::Quit => break,
            };
            let id = matches!(cmd, ClientCmd::GetContext | ClientCmd::Export { .. }).then(|| "ui".to_string());
            if req_tx.send(Request { id, cmd }).is_err() {
                break;
            }
        }
    });
    let cwd = PathBuf::from(&state.cwd);
    let init = novacode_tui::Init {
        model: state.model.clone(),
        branch: crate::git_branch(&cwd),
        cwd: state.cwd.clone(),
        window_tokens: state.window_tokens,
        history: messages,
        keymap: base.keymap,
        collapse_turns: base.collapse_turns,
        info: vec![format!("attached to session {id}: leaving only detaches")],
        notices: base.notices,
        theme: base.theme,
        thinking: base.thinking,
        level: state.level,
        price: state.price,
        spent: state.spent,
        compactions: state.compactions,
        commands: vec![],
        draft: String::new(),
        attached: true,
        statusline: base.statusline,
        panel_only,
        panel_auto: base.panel_auto,
        nerd_icons: base.nerd_icons,
        slow: base.slow,
        images: base.images.clone(),
        procs: Vec::new(),
    };
    novacode_tui::run(init, erx, ui_rx, cmd_tx).await?;
    match bye.lock().unwrap().take() {
        // The owner is rebuilding it: the caller reconnects (None).
        Some(reason) if reason == RELOADING => return Ok(None),
        Some(reason) => eprintln!("the session is over: {reason}"),
        None => eprintln!(
            "detached; the session keeps running (novacode attach {})",
            &id[..id.len().min(13)]
        ),
    }
    Ok(Some(std::process::ExitCode::SUCCESS))
}

/// `attach`/`panel`, reconnecting while the owner rebuilds the same session.
pub async fn attach_loop(
    paths: &Paths,
    prefix: &str,
    base: impl Fn() -> crate::TuiBase,
    panel_only: bool,
) -> anyhow::Result<std::process::ExitCode> {
    loop {
        if let Some(code) = attach(paths, prefix, base(), panel_only).await? {
            return Ok(code);
        }
        // The owner closes the old socket, then goes live again with the same id.
        let back = async {
            loop {
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                if find_live(paths, prefix).is_ok() {
                    break;
                }
            }
        };
        if tokio::time::timeout(std::time::Duration::from_secs(10), back)
            .await
            .is_err()
        {
            eprintln!("the session is over: it didn't come back after a reload");
            return Ok(std::process::ExitCode::SUCCESS);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// L11.14: the session address round-trips on every OS, for several clients in a row
    /// (Windows hands each its own pipe instance).
    #[tokio::test]
    async fn live_address_serves_each_client() {
        let dir = std::env::temp_dir().join(format!("nc-addr-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let sock = address(&dir, &format!("t{}", std::process::id()));
        let accept = listen(&sock, |r, mut w| {
            tokio::spawn(async move {
                let mut lines = BufReader::new(r).lines();
                while let Ok(Some(l)) = lines.next_line().await {
                    let _ = w.write_all(format!("{l}!\n").as_bytes()).await;
                }
            });
        })
        .unwrap();
        for n in 0..3 {
            let (r, mut w) = tokio::io::split(connect(&sock).await.unwrap());
            w.write_all(format!("hi{n}\n").as_bytes()).await.unwrap();
            let got = BufReader::new(r).lines().next_line().await.unwrap();
            assert_eq!(got, Some(format!("hi{n}!")));
        }
        // Another process can't take over a live session's pipe name.
        #[cfg(windows)]
        assert!(listen(&sock, |_, _| {}).is_err());
        accept.abort();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
