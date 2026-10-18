//! novacode binary: CLI, print mode (L8.3, L8.4). M2: agent loop with tools and saved sessions.

use anyhow::{Context, bail};
use clap::{Parser, Subcommand};
use futures_util::future::BoxFuture;
use novacode_ai::adapter::CacheRetention;
use novacode_ai::auth::{self, AuthStore, DEFAULT_ACCOUNT, antigravity, claude, codex, copilot};
use novacode_ai::connect::Providers;
use novacode_ai::trace::Trace;
use novacode_base::redact::Redactor;
use novacode_base::{logging, new_id, paths::Paths, settings, settings::Settings};
use novacode_core::agent::{self, Agent, Config};
use novacode_core::policy::{Approval, ApprovalRequest, Approver, Mode, Permissions, Policy};
use novacode_core::session::{self, Store};
use novacode_core::trust::{self, TrustStore};
use novacode_tools::LocalEnv;
use novacode_tui::dialog::{Answer, ApproveReq, PickItem};
use novacode_tui::keys::Keymap;
use novacode_tui::{Cmd, Exit, UiEvent};
use novacode_types::{AgentEvent, Message, StopReason, StreamEvent, ThinkingLevel};
use std::collections::BTreeMap;
use std::io::{IsTerminal, Read, Write};
use std::path::Path;
use std::process::ExitCode;
use std::sync::Arc;
use tokio::sync::{broadcast, mpsc, oneshot};

mod doctor;
mod models;

#[derive(Parser, Clone)]
#[command(name = "novacode", version, about = "An agent harness")]
struct Cli {
    /// Print mode: send PROMPT, print the final answer to stdout, exit.
    #[arg(short, long, value_name = "PROMPT")]
    print: Option<String>,
    /// Model as provider/model, e.g. claude/claude-opus-5-5 (subscription) or anthropic/… (API key).
    #[arg(short, long)]
    model: Option<String>,
    /// Record the raw request and stream (redacted) to <home>/logs/trace-<id>.jsonl.
    #[arg(long)]
    trace: bool,
    /// Report tool calls, stop reason, token/cache usage and the session file on stderr.
    #[arg(short, long)]
    verbose: bool,
    /// Continue the most recent session in this folder.
    #[arg(short, long)]
    r#continue: bool,
    /// Resume a session by id prefix.
    #[arg(short, long, value_name = "ID")]
    resume: Option<String>,
    /// Don't save this session.
    #[arg(long)]
    no_session: bool,
    /// Permission mode for this session: auto, ask or read-only (L6.1).
    #[arg(long, value_parser = parse_mode)]
    mode: Option<Mode>,
    /// Restrict tools to a named profile, e.g. scout (L6.4).
    #[arg(long)]
    profile: Option<String>,
    /// Trust this folder's project config for this run only (L6.3).
    #[arg(long)]
    trust: bool,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand, Clone)]
enum AuthCmd {
    /// Each login and key: where it comes from and whether its token is valid.
    Status,
}

#[derive(Subcommand, Clone)]
enum Command {
    /// Log in to a subscription provider (claude, codex, antigravity, copilot).
    /// Several accounts per provider (--account work) rotate when one runs out (L2.14).
    Login {
        provider: String,
        #[arg(long, default_value = DEFAULT_ACCOUNT)]
        account: String,
    },
    /// Remove a stored login (one account with --account, else all of them).
    Logout {
        provider: String,
        #[arg(long)]
        account: Option<String>,
    },
    /// List models of connected providers (with a filter); --all lists every provider; --refresh updates the catalog.
    Models {
        filter: Option<String>,
        #[arg(long)]
        all: bool,
        #[arg(long)]
        refresh: bool,
    },
    /// Show logins and keys (`auth status`).
    Auth {
        #[command(subcommand)]
        what: AuthCmd,
    },
    /// Check every connected provider, settings and key bindings.
    Doctor,
    /// Trust a folder's project config (default: the current folder); --revoke to distrust it.
    Trust {
        path: Option<std::path::PathBuf>,
        #[arg(long)]
        revoke: bool,
    },
}

fn parse_mode(s: &str) -> Result<Mode, String> {
    serde_json::from_value(serde_json::Value::from(s)).map_err(|_| format!("`{s}`: use auto, ask or read-only"))
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let paths = match Paths::from_env() {
        Ok(p) => p,
        Err(e) => return fail(e.into()),
    };
    let _log = match logging::init(&paths, cli.trace) {
        Ok(g) => g,
        Err(e) => return fail(e.into()),
    };
    let rt = match tokio::runtime::Runtime::new() {
        Ok(rt) => rt,
        Err(e) => return fail(e.into()),
    };
    match rt.block_on(run(cli, paths)) {
        Ok(code) => code,
        Err(e) => fail(e),
    }
}

fn fail(e: anyhow::Error) -> ExitCode {
    eprintln!("novacode: {e:#}");
    ExitCode::from(1)
}

async fn run(cli: Cli, paths: Paths) -> anyhow::Result<ExitCode> {
    let auth_store = AuthStore::new(paths.home().join("auth.json"));
    match cli.command {
        Some(Command::Login { provider, account }) => return login(&auth_store, &provider, &account).await,
        Some(Command::Logout { provider, account }) => {
            let had = auth_store.remove(&provider, account.as_deref()).await?;
            eprintln!(
                "{}",
                if had {
                    format!("logged out of {provider}")
                } else {
                    format!("no login for {provider}")
                }
            );
            return Ok(ExitCode::SUCCESS);
        }
        Some(Command::Auth { what: AuthCmd::Status }) | Some(Command::Doctor) => {
            let cwd = std::env::current_dir()?;
            let trusted = trust::resolve(&TrustStore::new(paths.home().join("trust.json")), &cwd, false);
            let settings = Settings::load(&paths.settings(), Some(&Paths::project_settings(&cwd)), trusted);
            let p = Arc::new(Providers::load(paths.home(), &settings));
            if matches!(cli.command, Some(Command::Doctor)) {
                let (km, mut problems) = load_keymap(&paths);
                problems.extend(km.conflicts());
                return Ok(doctor::doctor(&p, &settings, problems).await);
            }
            return Ok(doctor::auth_status(&p));
        }
        Some(Command::Models { filter, all, refresh }) => {
            let cache = novacode_ai::connect::cache_path(paths.home());
            if refresh {
                match novacode_ai::catalog::refresh(&cache).await {
                    Ok(true) => eprintln!("catalog updated from models.dev"),
                    Ok(false) => eprintln!("catalog already up to date"),
                    Err(e) => eprintln!("warning: catalog refresh failed: {e}"),
                }
            }
            let cwd = std::env::current_dir()?;
            let trusted = trust::resolve(&TrustStore::new(paths.home().join("trust.json")), &cwd, false);
            let settings = Settings::load(&paths.settings(), Some(&Paths::project_settings(&cwd)), trusted);
            return models::command(&Arc::new(Providers::load(paths.home(), &settings)), filter, all).await;
        }
        Some(Command::Trust { path, revoke }) => {
            let dir = std::fs::canonicalize(path.unwrap_or(std::env::current_dir()?))?;
            let trust = TrustStore::new(paths.home().join("trust.json"));
            if revoke {
                trust.set(&dir, false)?;
                eprintln!("distrusted {} (its project config won't load)", dir.display());
            } else {
                trust.set(&dir, true)?;
                eprintln!("trusted {} and the folders below it", dir.display());
            }
            return Ok(ExitCode::SUCCESS);
        }
        None => {}
    }
    let mut prompt = cli.print.clone();
    let stdin = std::io::stdin();
    if let Some(p) = &mut prompt {
        if !stdin.is_terminal() {
            let mut piped = String::new();
            stdin.lock().read_to_string(&mut piped)?;
            if !piped.trim().is_empty() {
                *p = format!("{p}\n\n{piped}");
            }
        }
    } else if !stdin.is_terminal() || !std::io::stdout().is_terminal() {
        bail!("interactive mode needs a terminal; use -p PROMPT for pipes and scripts");
    }
    let interactive = prompt.is_none();

    let cwd = std::env::current_dir()?;
    let trust_store = TrustStore::new(paths.home().join("trust.json"));
    let mut trusted = trust::resolve(&trust_store, &cwd, cli.trust);
    if !trusted && trust::has_project_config(&cwd) {
        if interactive && trust_store.decision(&cwd).is_none() {
            trusted = ask_trust(&trust_store, &cwd)?;
        } else {
            eprintln!(
                "note: this folder's project config is ignored (not trusted). Run `novacode trust` or pass --trust."
            );
        }
    }
    let settings = Settings::load(&paths.settings(), Some(&Paths::project_settings(&cwd)), trusted);
    let providers = Arc::new(Providers::load(paths.home(), &settings));
    let model = match cli.model.clone().or_else(|| settings.get::<String>("model")) {
        Some(m) => m,
        None if interactive => onboard(&paths, &providers).await?,
        None => bail!("no model: pass -m provider/model or set \"model\" in settings"),
    };
    // L2.10: refresh model metadata in the background; used from the next start.
    let cache = novacode_ai::connect::cache_path(paths.home());
    if interactive && novacode_ai::catalog::stale(&cache) {
        tokio::spawn(async move {
            if let Err(e) = novacode_ai::catalog::refresh(&cache).await {
                tracing::warn!(error = %e, "catalog refresh failed");
            }
        });
    }
    drop(auth_store);
    let ctx = Ctx {
        quirks: Arc::new(novacode_ai::schema::Quirks::load(paths.home())),
        paths,
        cwd,
        settings,
        providers,
        cli,
    };
    let store = if let Some(prefix) = &ctx.cli.resume {
        Store::open(&session::find(&ctx.paths, &ctx.cwd, prefix)?)?
    } else if ctx.cli.r#continue {
        let last = session::list(&ctx.paths, &ctx.cwd)
            .into_iter()
            .next()
            .context("no earlier session in this folder")?;
        Store::open(&last)?
    } else {
        Store::create(&ctx.paths, &ctx.cwd, ctx.cli.no_session)
    };
    match prompt {
        None => interactive_loop(&ctx, store, model).await,
        Some(prompt) => {
            // Nobody to approve: asks become refusals, risky commands need a justification.
            let (agent, _) = ctx.agent(store, &model, None).await?;
            print_mode(agent, prompt, ctx.cwd.clone(), ctx.cli.verbose).await
        }
    }
}

/// Everything needed to build an agent for a session.
struct Ctx {
    paths: Paths,
    cwd: std::path::PathBuf,
    settings: Settings,
    providers: Arc<Providers>,
    quirks: Arc<novacode_ai::schema::Quirks>,
    cli: Cli,
}

impl Ctx {
    fn trace(&self) -> Option<Trace> {
        self.cli.trace.then(|| Trace {
            path: self.paths.logs().join(format!("trace-{}.jsonl", new_id())),
        })
    }

    /// Build the agent for `model` (`provider/model[:level]`). Returns the resolved `provider/model`.
    async fn agent(
        &self,
        store: Store,
        model: &str,
        approver: Option<Arc<dyn Approver>>,
    ) -> anyhow::Result<(Agent, String)> {
        let route = self.providers.resolve(model)?;
        // `model:level` wins, then the session's own level (resume), then settings.
        let level = route
            .level
            .or(store.last_thinking())
            .or_else(|| {
                self.settings
                    .get::<String>("thinking")
                    .and_then(|l| ThinkingLevel::parse(&l))
            })
            .unwrap_or_default();
        let trace = self.trace();
        if let Some(t) = &trace
            && approver.is_none()
        {
            eprintln!("trace: {}", t.path.display());
        }
        let (adapter, api_key) = self.providers.adapter(&route.provider, trace).await?;
        let s = &self.settings;
        let perms: Permissions = s.get("permissions").unwrap_or_default();
        let mut policy = Policy::new(
            perms,
            std::env::var_os("HOME").map(Into::into).unwrap_or_default(),
            Some(self.paths.settings()),
        );
        if let Some(m) = self.cli.mode {
            policy = policy.with_mode(m);
        }
        if let Some(p) = &self.cli.profile {
            policy = policy.with_profile(p).map_err(anyhow::Error::msg)?;
        }
        let (window_tokens, max_tokens) = models::Limits::new(s).of(&route);
        let cfg = Config {
            cwd: self.cwd.clone(),
            provider: route.provider.clone(),
            model: route.model.clone(),
            max_tokens,
            cache: match s.get::<String>("cacheRetention").as_deref() {
                Some("long") => CacheRetention::Long,
                _ => CacheRetention::Short,
            },
            window_tokens,
            thinking: route.clamp(level),
            images: route.info.images,
            reasoning: route.info.reasoning,
            spill_dir: self.paths.home().join("spill"),
            redactor: {
                let auth_path = self.paths.home().join("auth.json");
                Redactor::new(move || {
                    let mut s = AuthStore::new(auth_path.clone()).secrets();
                    s.extend(api_key.clone().map(|k| (k, "API key".to_string())));
                    s
                })
            },
        };
        let mut agent = Agent::new(adapter, novacode_tools::builtins(false), store, cfg, Arc::new(LocalEnv))
            .with_policy(policy)
            .with_quirks(self.quirks.clone());
        if let Some(a) = approver {
            agent = agent.with_approver(a);
        }
        Ok((agent, route.spec()))
    }
}

/// Attach the images a prompt mentions (`@shot.png`, L2.22), off the async threads.
async fn attach(text: String, cwd: std::path::PathBuf) -> (Vec<novacode_types::Block>, Vec<String>) {
    let fallback = text.clone();
    tokio::task::spawn_blocking(move || novacode_tools::image::attach(&text, &cwd))
        .await
        .unwrap_or_else(|_| (vec![novacode_types::Block::text(fallback)], vec![]))
}

async fn print_mode(agent: Agent, prompt: String, cwd: std::path::PathBuf, verbose: bool) -> anyhow::Result<ExitCode> {
    let (handle, task) = agent::spawn(agent);
    let mut events = handle.subscribe();
    let on_ctrl_c = handle.clone();
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            on_ctrl_c.abort();
        }
    });
    let (blocks, notes) = attach(prompt, cwd).await;
    for n in notes {
        eprintln!("{n}");
    }
    handle.prompt_content(blocks);
    // stdout gets only the final answer (L8.4); progress goes to stderr with -v.
    let mut last = None;
    let stop = loop {
        match events.recv().await {
            Ok(AgentEvent::Message(Message::Assistant(m))) => last = Some(m),
            Ok(AgentEvent::ToolStart { call }) if verbose => eprintln!("tool {} {}", call.name, call.arguments),
            Ok(AgentEvent::Stream(StreamEvent::Notice(text))) => eprintln!("{text}"),
            Ok(AgentEvent::Retry {
                attempt,
                delay_ms,
                message,
                ..
            }) => {
                eprintln!("retry {attempt} in {delay_ms}ms: {message}")
            }
            Ok(AgentEvent::AgentEnd { stop }) => break stop,
            Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {}
            Err(broadcast::error::RecvError::Closed) => bail!("session ended unexpectedly"),
        }
    };
    handle.shutdown();
    let agent = task.await?;
    let msg = last.context("no reply")?;
    tracing::info!(stop = ?stop, usage = ?msg.usage, "print mode done");
    let text = msg.text();
    if !text.is_empty() {
        let mut out = std::io::stdout().lock();
        writeln!(out, "{text}")?;
        out.flush()?;
    }
    if verbose {
        let u = msg.usage;
        eprintln!(
            "stop={:?} input={} output={} cache_read={} cache_write={}",
            stop, u.input, u.output, u.cache_read, u.cache_write
        );
        match agent.store.path() {
            Some(p) => eprintln!("session: {}", p.display()),
            None => eprintln!("session: not saved"),
        }
    }
    Ok(match stop {
        StopReason::Stop | StopReason::ToolUse | StopReason::Length => ExitCode::SUCCESS,
        StopReason::Aborted => ExitCode::from(130),
        StopReason::Error { kind } => {
            eprintln!("novacode: {kind:?}: {}", msg.error_message.unwrap_or_default());
            ExitCode::from(1)
        }
    })
}

/// Sends approval questions to the TUI and waits for the answer (L6.1).
struct TuiApprover(mpsc::UnboundedSender<UiEvent>);

impl Approver for TuiApprover {
    fn approve(&self, req: ApprovalRequest) -> BoxFuture<'_, Approval> {
        Box::pin(async move {
            let (tx, rx) = oneshot::channel();
            let detail = match req.args.get("command").or_else(|| req.args.get("path")) {
                Some(serde_json::Value::String(s)) => s.clone(),
                _ => req.args.to_string(),
            };
            let card = ApproveReq {
                tool: req.tool,
                detail,
                reason: req.reason,
                rule: req.rule,
                risky: req.risky,
                reply: tx,
            };
            if self.0.send(UiEvent::Approve(card)).is_err() {
                return Approval::Deny(None);
            }
            match rx.await {
                Ok(Answer::Once) => Approval::Once,
                Ok(Answer::Session) => Approval::Session,
                Ok(Answer::Always) => Approval::Always,
                Ok(Answer::Deny(why)) => Approval::Deny(why),
                Err(_) => Approval::Deny(None),
            }
        })
    }
}

fn load_keymap(paths: &Paths) -> (Keymap, Vec<String>) {
    let path = paths.home().join("keybindings.json");
    let map = match std::fs::read_to_string(&path) {
        Ok(s) => match serde_json::from_str::<BTreeMap<String, serde_json::Value>>(&s) {
            Ok(m) => m,
            Err(e) => {
                let (km, _) = Keymap::new(&BTreeMap::new());
                return (
                    km,
                    vec![format!(
                        "{} is not valid JSON ({e}); using default keys",
                        path.display()
                    )],
                );
            }
        },
        Err(_) => BTreeMap::new(),
    };
    Keymap::new(&map)
}

fn ago(t: std::time::SystemTime) -> String {
    let s = t.elapsed().map_or(0, |d| d.as_secs());
    match s {
        s if s < 60 => "just now".into(),
        s if s < 3600 => format!("{}m ago", s / 60),
        s if s < 86_400 => format!("{}h ago", s / 3600),
        s => format!("{}d ago", s / 86_400),
    }
}

/// The TUI, one session at a time; /resume and /new swap the session in place.
async fn interactive_loop(ctx: &Ctx, mut store: Store, mut model: String) -> anyhow::Result<ExitCode> {
    let mut notices = Vec::new();
    loop {
        let (ui_tx, ui_rx) = mpsc::unbounded_channel();
        let approver: Arc<dyn Approver> = Arc::new(TuiApprover(ui_tx.clone()));
        let (agent, spec) = ctx.agent(store, &model, Some(approver)).await?;
        let history = agent.store.projection();
        let level = agent.config().thinking;
        let window_tokens = agent.config().window_tokens;
        let price = ctx
            .providers
            .resolve(&spec)
            .ok()
            .and_then(|r| models::price(&ctx.providers, &r));
        let spent = models::spent(&ctx.providers, &history);
        let id = agent.store.id().to_string();
        let (handle, task) = agent::spawn(agent);
        let (etx, erx) = mpsc::unbounded_channel();
        let mut events = handle.subscribe();
        tokio::spawn(async move {
            loop {
                match events.recv().await {
                    Ok(e) => {
                        if etx.send(e).is_err() {
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => tracing::warn!(n, "ui missed events"),
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        });
        let (ctx_tx, mut crx) = mpsc::unbounded_channel();
        let h = handle.clone();
        // The resolved `provider/model` and the level in use, kept for /resume and /new.
        let current = Arc::new(std::sync::Mutex::new((spec.clone(), level)));
        let cur = current.clone();
        let (paths, cwd, this_id) = (ctx.paths.clone(), ctx.cwd.clone(), id.clone());
        let (providers, limits, trace) = (ctx.providers.clone(), models::Limits::new(&ctx.settings), ctx.trace());
        tokio::spawn(async move {
            while let Some(c) = crx.recv().await {
                let steer = matches!(c, Cmd::Steer(_));
                match c {
                    Cmd::Prompt(t) | Cmd::Steer(t) => {
                        let (blocks, notes) = attach(t, cwd.clone()).await;
                        for n in notes {
                            let error = !n.starts_with("attached");
                            let _ = ui_tx.send(UiEvent::Notice { text: n, error });
                        }
                        if steer {
                            h.steer_content(blocks)
                        } else {
                            h.prompt_content(blocks)
                        }
                    }
                    Cmd::Abort => h.abort(),
                    Cmd::SetModel(m) => {
                        let (p, tx, cur, h, trace) =
                            (providers.clone(), ui_tx.clone(), cur.clone(), h.clone(), trace.clone());
                        tokio::spawn(async move {
                            let level = cur.lock().unwrap().1;
                            match models::switch(&p, &m, level, limits, trace).await {
                                Ok((sw, route)) => {
                                    let ev = UiEvent::ModelSet {
                                        model: route.spec(),
                                        window_tokens: sw.window_tokens,
                                        level: sw.thinking,
                                        price: models::price(&p, &route),
                                    };
                                    *cur.lock().unwrap() = (route.spec(), sw.thinking);
                                    h.set_model(sw);
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
                        let spec = cur.lock().unwrap().0.clone();
                        let Ok(route) = providers.resolve(&spec) else { continue };
                        let level = route.clamp(l);
                        cur.lock().unwrap().1 = level;
                        h.set_thinking(level);
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
                        let _ = ui_tx.send(UiEvent::Thinking { level, note });
                    }
                    Cmd::ListModels => {
                        let (p, tx, cur) = (providers.clone(), ui_tx.clone(), cur.lock().unwrap().0.clone());
                        tokio::spawn(async move {
                            let (items, errors) = models::list(&p, &cur).await;
                            for e in errors {
                                let _ = tx.send(UiEvent::Notice {
                                    text: format!("models: {e}"),
                                    error: true,
                                });
                            }
                            let r = if items.is_empty() {
                                Err("no connected provider listed any model".into())
                            } else {
                                Ok(items)
                            };
                            let _ = tx.send(UiEvent::Models(r));
                        });
                    }
                    Cmd::ListSessions => {
                        let items = session::list(&paths, &cwd)
                            .iter()
                            .filter_map(|p| session::summary(p))
                            .filter(|s| s.id != this_id && s.prompts > 0)
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
                        let _ = ui_tx.send(UiEvent::Sessions(items));
                    }
                    Cmd::Quit => break,
                }
            }
        });
        let init = novacode_tui::Init {
            model: spec.clone(),
            cwd: ctx.cwd.display().to_string(),
            branch: git_branch(&ctx.cwd),
            window_tokens,
            history,
            keymap: {
                let (km, warn) = load_keymap(&ctx.paths);
                notices.extend(warn);
                km
            },
            collapse_turns: ctx.settings.get("ui.turnCollapse").unwrap_or(true),
            level,
            thinking: novacode_tui::transcript::Thinking::parse(
                &ctx.settings.get::<String>("ui.thinking").unwrap_or_default(),
            ),
            theme: {
                let name = ctx.settings.get::<String>("ui.theme").unwrap_or_else(|| "auto".into());
                let dark = (name == "auto").then(novacode_tui::term::query_background).flatten();
                let (mut t, warn) = novacode_tui::theme::Theme::load(&name, &ctx.paths.home().join("themes"), dark);
                notices.extend(warn);
                if let Some(v) = ctx.settings.get::<serde_json::Value>("ui.userMessage") {
                    notices.extend(t.apply_user_message(&v));
                }
                t
            },
            notices: std::mem::take(&mut notices),
            price,
            spent,
        };
        let res = novacode_tui::run(init, erx, ui_rx, ctx_tx).await;
        handle.abort();
        handle.shutdown();
        let agent = task.await?;
        // /new and /resume keep the model; the level comes from the session (or settings).
        model = current.lock().unwrap().0.clone();
        let prev = agent.store.path().map(Path::to_path_buf);
        drop(agent);
        let next = match res? {
            Exit::Quit => {
                if prev.as_ref().is_some_and(|p| p.exists()) {
                    eprintln!("resume with: novacode -r {}", &id[..id.len().min(13)]);
                }
                return Ok(ExitCode::SUCCESS);
            }
            Exit::New => Ok(Store::create(&ctx.paths, &ctx.cwd, ctx.cli.no_session)),
            Exit::Resume(path) => Store::open(Path::new(&path)),
        };
        store = match next {
            Ok(s) => s,
            Err(e) => {
                notices.push(format!("could not open that session: {e}"));
                match prev.filter(|p| p.exists()) {
                    Some(p) => Store::open(&p)?,
                    None => Store::create(&ctx.paths, &ctx.cwd, ctx.cli.no_session),
                }
            }
        };
    }
}

/// L6.3: four choices, asked once per folder that has project config.
fn ask_trust(store: &TrustStore, cwd: &Path) -> anyhow::Result<bool> {
    let parent = cwd.parent().unwrap_or(cwd).to_path_buf();
    let item = |label: String, value: &str| PickItem {
        label,
        detail: String::new(),
        value: value.into(),
    };
    let body = [
        format!(
            "{} has project config (.novacode/settings.json, extensions, MCP, hooks or skills).",
            cwd.display()
        ),
        "Trusting it lets that config change settings and run code. AGENTS.md and CLAUDE.md load either way.".into(),
    ];
    let pick = novacode_tui::prompt::select(
        "Trust this folder?",
        &body,
        vec![
            item("Trust this folder".into(), "this"),
            item(format!("Trust the parent folder {}", parent.display()), "parent"),
            item("Trust for this session only".into(), "session"),
            item("Don't trust".into(), "never"),
        ],
    )?;
    Ok(match pick.as_deref() {
        Some("this") => {
            store.set(cwd, true)?;
            true
        }
        Some("parent") => {
            store.set(&parent, true)?;
            true
        }
        Some("session") => true,
        Some("never") => {
            store.set(cwd, false)?;
            false
        }
        _ => false,
    })
}

/// L11.6 first run: connect (subscription first), pick a model, save it.
async fn onboard(paths: &Paths, p: &Arc<Providers>) -> anyhow::Result<String> {
    let item = |label: String, detail: String, value: &str| PickItem {
        label,
        detail,
        value: value.into(),
    };
    let mut items = Vec::new();
    // Subscriptions first, then every provider whose key is already set.
    for pr in p.profiles.values().filter(|pr| pr.oauth()) {
        let state = if p.connected(pr) {
            "logged in"
        } else {
            "log in with your browser"
        };
        items.push(item(format!("{} subscription", pr.name), state.into(), &pr.id));
    }
    for pr in p
        .profiles
        .values()
        .filter(|pr| !pr.oauth() && !pr.key_optional && p.connected(pr))
    {
        let how = pr
            .api_key
            .as_ref()
            .map_or_else(|| format!("found {}", pr.env.join("/")), |_| "key in settings".into());
        items.push(item(pr.name.clone(), how, &pr.id));
    }
    let body = [
        "Welcome to novacode. How should it reach a model?".to_string(),
        "API-key providers appear here once their key env var is set (see `novacode models --all`).".into(),
    ];
    let provider = novacode_tui::prompt::select("Connect", &body, items)?
        .context("setup cancelled; run novacode again, or pass -m provider/model")?;
    let pr = p.profile(&provider)?;
    if pr.oauth() && !p.connected(pr) {
        login(&p.auth, &provider, DEFAULT_ACCOUNT).await?;
    }
    let (adapter, _) = p.adapter(&provider, None).await?;
    let ids = adapter
        .list_models()
        .await
        .map_err(|e| anyhow::anyhow!("could not list models: {e}. Pass -m {provider}/<model> instead."))?;
    let items = ids
        .into_iter()
        .map(|id| item(id.clone(), String::new(), &format!("{provider}/{id}")))
        .collect();
    let body = ["Pick a default model. It is saved to settings.json; /model changes it later.".to_string()];
    let model = novacode_tui::prompt::select("Model", &body, items)?.context("setup cancelled")?;
    settings::set(&paths.settings(), "model", &serde_json::Value::from(model.clone()))?;
    eprintln!("saved \"model\": \"{model}\" to {}", paths.settings().display());
    Ok(model)
}

/// Current branch (or short HEAD) from `.git`, worktrees included. No git process.
fn git_branch(cwd: &Path) -> Option<String> {
    let dot = cwd.ancestors().map(|d| d.join(".git")).find(|p| p.exists())?;
    let dir = if dot.is_file() {
        let s = std::fs::read_to_string(&dot).ok()?;
        let g = std::path::PathBuf::from(s.trim().strip_prefix("gitdir: ")?);
        if g.is_absolute() { g } else { dot.parent()?.join(g) }
    } else {
        dot
    };
    let head = std::fs::read_to_string(dir.join("HEAD")).ok()?;
    let head = head.trim();
    Some(match head.strip_prefix("ref: refs/heads/") {
        Some(b) => b.to_string(),
        None => head.chars().take(8).collect(),
    })
}

async fn login(store: &AuthStore, provider: &str, account: &str) -> anyhow::Result<ExitCode> {
    // Default browser via xdg-open; the URL is always printed too, for headless or another browser.
    let open = |url: &str| {
        let _ = std::process::Command::new("xdg-open")
            .arg(url)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
    };
    let cred = match provider {
        claude::PROVIDER => {
            let (verifier, challenge) = auth::pkce();
            let url = claude::authorize_url(&verifier, &challenge);
            eprintln!("Open this URL, sign in, then paste the code shown:\n\n{url}\n");
            open(&url);
            eprint!("code: ");
            let mut line = String::new();
            std::io::stdin().read_line(&mut line)?;
            claude::exchange(&line, &verifier).await?
        }
        codex::PROVIDER => {
            let d = codex::start().await?;
            eprintln!(
                "Open {} and enter the code\n\n    {}\n\nWaiting for you to approve (Ctrl+C to cancel)…",
                codex::VERIFY_URL,
                d.user_code
            );
            open(codex::VERIFY_URL);
            codex::finish(&d).await?
        }
        antigravity::PROVIDER => {
            let (verifier, challenge) = auth::pkce();
            let state = auth::pkce().0;
            let url = antigravity::authorize_url(&challenge, &state);
            eprintln!(
                "Open this URL and sign in with Google:\n\n{url}\n\nnovacode catches the redirect on localhost:{}. On another machine, paste the URL the browser ends on:",
                antigravity::PORT
            );
            open(&url);
            let (code, got_state) = loopback_or_paste(antigravity::PORT).await?;
            if got_state.as_deref() != Some(state.as_str()) {
                bail!("OAuth state mismatch; start the login again");
            }
            antigravity::exchange(&code, &verifier).await?
        }
        copilot::PROVIDER => {
            let d = copilot::start().await?;
            eprintln!(
                "Open {} and enter the code\n\n    {}\n\nWaiting for you to approve (Ctrl+C to cancel)…",
                copilot::VERIFY_URL,
                d.user_code
            );
            open(copilot::VERIFY_URL);
            let c = copilot::finish(&d).await?;
            let on = copilot::enable_models(&c.access).await;
            if !on.is_empty() {
                eprintln!("enabled models: {}", on.join(", "));
            }
            c
        }
        _ => bail!("unknown login provider `{provider}` (logins: claude, codex, antigravity, copilot)"),
    };
    store.put(provider, account, cred).await?;
    if account == DEFAULT_ACCOUNT {
        eprintln!("logged in to {provider}");
    } else {
        eprintln!("logged in to {provider} (account `{account}`)");
    }
    Ok(ExitCode::SUCCESS)
}

/// Wait for an OAuth redirect on `127.0.0.1:port`, or a pasted redirect URL on stdin,
/// whichever comes first (L2.13). Returns (code, state).
async fn loopback_or_paste(port: u16) -> anyhow::Result<(String, Option<String>)> {
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await.ok();
    let caught = async {
        let Some(l) = listener else {
            return std::future::pending().await;
        };
        loop {
            let Ok((mut sock, _)) = l.accept().await else { continue };
            let mut buf = vec![0u8; 8192];
            let n = sock.read(&mut buf).await.unwrap_or(0);
            let req = String::from_utf8_lossy(&buf[..n]).to_string();
            let target = req.split_whitespace().nth(1).unwrap_or_default().to_string();
            if !target.contains("code=") {
                let _ = sock
                    .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n")
                    .await;
                continue;
            }
            let page = "<html><body><h3>novacode is logged in. You can close this tab.</h3></body></html>";
            let reply = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{page}",
                page.len()
            );
            let _ = sock.write_all(reply.as_bytes()).await;
            return target;
        }
    };
    let pasted = async {
        let mut line = String::new();
        let _ = BufReader::new(tokio::io::stdin()).read_line(&mut line).await;
        line
    };
    let got = tokio::select! {
        t = caught => t,
        l = pasted => l,
    };
    let (code, state) = claude::parse_pasted(&got);
    if code.is_empty() {
        bail!("no authorization code received");
    }
    Ok((percent_decode(&code), state.map(|s| percent_decode(&s))))
}

/// `4%2F0Ab` → `4/0Ab` (query values in a redirect URL).
fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match (
            b[i],
            b.get(i + 1..i + 3)
                .and_then(|h| u8::from_str_radix(std::str::from_utf8(h).ok()?, 16).ok()),
        ) {
            (b'%', Some(v)) => {
                out.push(v);
                i += 3;
            }
            (b'+', _) => {
                out.push(b' ');
                i += 1;
            }
            (c, _) => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    #[test]
    fn percent_decoding() {
        assert_eq!(super::percent_decode("4%2F0Ab-x%3D"), "4/0Ab-x=");
        assert_eq!(super::percent_decode("plain%zz"), "plain%zz");
    }
}
