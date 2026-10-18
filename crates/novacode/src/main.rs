//! novacode binary: CLI, print mode (L8.3, L8.4). M2: agent loop with tools and saved sessions.

use anyhow::{Context, bail};
use clap::{Parser, Subcommand};
use novacode_ai::adapter::CacheRetention;
use novacode_ai::auth::{self, AuthStore, DEFAULT_ACCOUNT, antigravity, claude, codex, copilot};
use novacode_ai::connect::Providers;
use novacode_ai::trace::Trace;
use novacode_base::redact::Redactor;
use novacode_base::{logging, new_id, paths::Paths, settings, settings::Settings};
use novacode_core::agent::{self, Agent, Config};
use novacode_core::policy::{Approver, Mode, Permissions, Policy};
use novacode_core::resources;
use novacode_core::session::{self, Store};
use novacode_core::trust::{self, TrustStore};
use novacode_tools::LocalEnv;
use novacode_tui::dialog::PickItem;
use novacode_tui::keys::Keymap;
use novacode_tui::{Cmd, Exit, UiEvent};
use novacode_types::{AgentEvent, Message, StopReason, StreamEvent, ThinkingLevel};
use std::collections::BTreeMap;
use std::io::{IsTerminal, Read, Write};
use std::path::Path;
use std::process::ExitCode;
use std::sync::Arc;
use tokio::sync::{broadcast, mpsc};

mod activity;
mod doctor;
mod host;
mod hub;
mod models;
mod serve;
mod stats;
mod status;
mod wire;

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
    /// Don't load AGENTS.md / CLAUDE.md context files (L4.9).
    #[arg(long)]
    no_context_files: bool,
    /// With -p: write the wire protocol's JSONL events to stdout instead of the answer (L8.5).
    #[arg(long, requires = "print")]
    json: bool,
    /// Serve the wire protocol on stdin/stdout: JSONL commands in, events out (L8.6).
    #[arg(long, conflicts_with = "print")]
    rpc: bool,
    /// Run in the background with no window; prints the session id. Attach with `novacode attach` (L8.13).
    #[arg(long, value_name = "PROMPT", num_args = 0..=1, default_missing_value = "", conflicts_with_all = ["print", "rpc"])]
    bg: Option<String>,
    /// API key for the model's provider, for this run only (ahead of saved and env keys).
    #[arg(long, value_name = "KEY")]
    api_key: Option<String>,
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
    /// Log in to a subscription (claude, codex, antigravity, copilot), or save an
    /// API key for any other provider (openrouter, openai, google, …).
    /// Several subscription accounts (--account work) rotate when one runs out (L2.14).
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
    /// Export a session's current branch (default: the newest here) as JSONL, or --html.
    Export {
        session: Option<String>,
        #[arg(long)]
        html: bool,
        /// Output file (default: novacode-<id>.jsonl|html in the current folder).
        #[arg(short, long)]
        out: Option<std::path::PathBuf>,
    },
    /// Open another window on a live session (leaving only detaches)
    Attach { session: String },
    /// List live sessions on this machine
    Agents,
    /// End a live session, a background one included
    Stop { session: String },
    /// Status of a live session: this pane's, else this folder's newest (L8.15)
    Status {
        /// Session id prefix.
        #[arg(long)]
        session: Option<String>,
        /// Every live session.
        #[arg(long)]
        all: bool,
        /// The full record as JSON lines.
        #[arg(long)]
        json: bool,
        /// Template, e.g. '{ctx}% {5h}'. Fields: session state detail model level ctx ctx_tokens window hit read write miss cost 5h week branch project cwd bg.
        #[arg(long, value_name = "TEMPLATE")]
        format: Option<String>,
    },
    /// Dev metrics report (needs `debug.metrics`): the latest session, one by id prefix, or --all.
    Stats {
        session: Option<String>,
        #[arg(long)]
        all: bool,
        /// List every turn, not only the slowest.
        #[arg(long)]
        turns: bool,
    },
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
        Some(Command::Login { provider, account }) => {
            let cwd = std::env::current_dir()?;
            let trusted = trust::resolve(&TrustStore::new(paths.home().join("trust.json")), &cwd, false);
            let settings = Settings::load(&paths.settings(), Some(&Paths::project_settings(&cwd)), trusted);
            let p = Providers::load(paths.home(), &settings);
            if p.profiles.get(&provider).is_some_and(|pr| !pr.oauth()) {
                if account != DEFAULT_ACCOUNT {
                    bail!("API keys don't rotate, so there is one per provider (no --account)");
                }
                return key_login(p, &provider).await;
            }
            return login(&auth_store, &provider, &account).await;
        }
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
        Some(Command::Export { session, html, out }) => {
            let cwd = std::env::current_dir()?;
            let file = match session {
                Some(p) => session::find(&paths, &cwd, &p)?,
                None => session::list(&paths, &cwd)
                    .into_iter()
                    .next()
                    .context("no sessions in this folder")?,
            };
            let out = export_session(&paths, &file, html, out)?;
            println!("{}", out.display());
            return Ok(ExitCode::SUCCESS);
        }
        Some(Command::Agents) => return Ok(serve::agents(&paths)),
        Some(Command::Status {
            session,
            all,
            json,
            format,
        }) => return Ok(status::run(&paths, session.as_deref(), all, json, format.as_deref())),
        Some(Command::Stop { session }) => return serve::stop(&paths, &session).await,
        Some(Command::Attach { session }) => {
            if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
                bail!("attach needs a terminal");
            }
            let cwd = std::env::current_dir()?;
            let trusted = trust::resolve(&TrustStore::new(paths.home().join("trust.json")), &cwd, false);
            let settings = Settings::load(&paths.settings(), Some(&Paths::project_settings(&cwd)), trusted);
            let base = tui_base(&paths, &settings);
            return serve::attach(&paths, &session, base).await;
        }
        Some(Command::Stats { session, all, turns }) => {
            std::process::exit(stats::run(paths.home(), session.as_deref(), all, turns));
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
    // --bg: start a detached copy of this command and report its session id.
    if cli.bg.is_some() && std::env::var_os("NOVACODE_BG_CHILD").is_none() {
        return spawn_bg();
    }
    let headless = cli.bg.is_some() || cli.rpc;
    let mut prompt = cli.print.clone();
    let stdin = std::io::stdin();
    if headless {
    } else if let Some(p) = &mut prompt {
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
    let interactive = prompt.is_none() && !headless;

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
    let mut providers = Providers::load(paths.home(), &settings);
    let chosen = cli.model.clone().or_else(|| settings.get::<String>("model"));
    if let Some(k) = cli.api_key.clone() {
        let Some(provider) = chosen
            .as_deref()
            .and_then(|m| m.split_once('/'))
            .map(|(p, _)| p.to_string())
        else {
            bail!("--api-key needs a model: pass -m provider/model");
        };
        providers.cli_key = Some((provider, k));
    }
    let providers = Arc::new(providers);
    let model = match chosen {
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
        trusted,
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
    if headless {
        return serve_headless(&ctx, store, model).await;
    }
    match prompt {
        None => interactive_loop(ctx, store, model).await,
        Some(prompt) => {
            // Nobody to approve: asks become refusals, risky commands need a justification.
            let (agent, _) = ctx.agent(store, &model, None).await?;
            print_mode(
                agent,
                prompt,
                ctx.cwd.clone(),
                ctx.inputs(),
                ctx.cli.verbose,
                ctx.cli.json,
            )
            .await
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
    /// Project config loads (L6.3): gates project skills.
    trusted: bool,
}

impl Ctx {
    /// `/reload` (L8.10): read trust, settings and provider config again. CLI flags stay.
    fn reload(&mut self) -> Vec<String> {
        // A `novacode trust` since start applies; no prompt here.
        self.trusted = trust::resolve(
            &TrustStore::new(self.paths.home().join("trust.json")),
            &self.cwd,
            self.cli.trust,
        );
        self.settings = Settings::load(
            &self.paths.settings(),
            Some(&Paths::project_settings(&self.cwd)),
            self.trusted,
        );
        let mut p = Providers::load(self.paths.home(), &self.settings);
        p.cli_key = self.providers.cli_key.clone();
        self.providers = Arc::new(p);
        self.quirks = Arc::new(novacode_ai::schema::Quirks::load(self.paths.home()));
        self.settings.diagnostics().into_iter().map(|d| d.message).collect()
    }

    /// Skill folders, first wins (L4.10): project (trusted only), novacode's, then the shared `~/.agents`.
    /// `.claude/skills` is never scanned (user requirement).
    fn skill_dirs(&self) -> Vec<std::path::PathBuf> {
        let mut d = Vec::new();
        if self.trusted {
            d.push(Paths::project_dir(&self.cwd).join("skills"));
        }
        d.push(self.paths.home().join("skills"));
        if let Some(h) = std::env::var_os("HOME") {
            d.push(Path::new(&h).join(".agents/skills"));
        }
        d
    }

    /// Prompt-template folders, project first (L4.11).
    fn template_dirs(&self) -> Vec<std::path::PathBuf> {
        vec![
            Paths::project_dir(&self.cwd).join("prompts"),
            self.paths.home().join("prompts"),
        ]
    }

    fn inputs(&self) -> Inputs {
        Inputs {
            skills: self.skill_dirs(),
            templates: self.template_dirs(),
        }
    }

    fn trace(&self) -> Option<Trace> {
        self.cli.trace.then(|| Trace {
            path: self.paths.logs().join(format!("trace-{}.jsonl", new_id())),
        })
    }

    /// The observer's model (L4.5): `memory.model`, else the provider's small
    /// model, else the session's own. A worker that can't connect falls back to it.
    async fn memory_worker(
        &self,
        route: &novacode_ai::connect::Route,
        main: Arc<dyn novacode_ai::adapter::Adapter>,
    ) -> novacode_core::memory::Worker {
        let small = self.providers.profile(&route.provider).ok().and_then(|p| {
            p.extra
                .get("smallModel")?
                .as_str()
                .map(|m| format!("{}/{m}", route.provider))
        });
        let spec = self.settings.get::<String>("memory.model").or(small);
        let fallback = || novacode_core::memory::Worker {
            adapter: main.clone(),
            provider: route.provider.clone(),
            model: route.model.clone(),
        };
        let Some(spec) = spec else { return fallback() };
        let r = match self.providers.resolve(&spec) {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(%e, "memory.model");
                return fallback();
            }
        };
        let adapter = if r.provider == route.provider {
            main.clone()
        } else {
            match self.providers.adapter(&r.provider, None).await {
                Ok((a, _)) => a,
                Err(e) => {
                    tracing::warn!(%e, "memory.model connect");
                    return fallback();
                }
            }
        };
        novacode_core::memory::Worker {
            adapter,
            provider: r.provider,
            model: r.model,
        }
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
            warm: self.settings.get("cache.warm").unwrap_or(false),
            metrics: self.settings.get("debug.metrics").unwrap_or(false),
            prompt: novacode_core::prompt::Sources {
                cwd: self.cwd.clone(),
                home: self.paths.home().to_path_buf(),
                context_files: !self.cli.no_context_files,
                skill_dirs: self.skill_dirs(),
                tool_guidelines: vec![],
            },
            redactor: {
                let auth_path = self.paths.home().join("auth.json");
                Redactor::new(move || {
                    let mut s = AuthStore::new(auth_path.clone()).secrets();
                    s.extend(api_key.clone().map(|k| (k, "API key".to_string())));
                    s
                })
            },
        };
        let memory_on = s.get("memory.enabled").unwrap_or(true);
        let mut all = novacode_tools::builtins(false);
        if store.path().is_some() {
            all.push(Arc::new(novacode_core::export::SessionSearch {
                sessions_root: self.paths.home().join("sessions"),
                project_dir: session::dir_for(&self.paths, &self.cwd),
                current: store.id().to_string(),
            }));
        }
        if memory_on && let Some(file) = store.path() {
            all.push(Arc::new(novacode_core::memory::Recall {
                file: file.to_path_buf(),
            }));
        }
        let tools = novacode_tools::listed(all, |name| self.settings.get(&format!("tools.{name}.exposure")));
        let window = cfg.window_tokens;
        let worker = memory_on.then(|| self.memory_worker(&route, adapter.clone()));
        let mut agent = Agent::new(adapter, tools, store, cfg, Arc::new(LocalEnv))
            .with_policy(policy)
            .with_quirks(self.quirks.clone());
        if s.get("checkpoints.enabled").unwrap_or(true)
            && agent.store.path().is_some()
            && let Some(repo) = novacode_core::checkpoint::resolve(&self.cwd, self.paths.home())
        {
            agent = agent.with_checkpoints(repo);
        }
        if let Some(w) = worker {
            let mut m = novacode_core::memory::Settings::for_window(window);
            if let Some(v) = s.get("memory.observeAfterTokens") {
                m.observe_after = v;
            }
            if let Some(v) = s.get("memory.keepRecentTokens") {
                m.keep_recent = v;
            }
            if let Some(v) = s.get("memory.compactAtTokens") {
                m.compact_at = v;
            }
            if let Some(v) = s.get("memory.reflectAfterTokens") {
                m.reflect_after = v;
            }
            if let Some(v) = s.get("memory.poolTargetTokens") {
                m.pool_target = v;
            }
            agent = agent.with_memory(w.await, m);
        }
        if let Some(a) = approver {
            agent = agent.with_approver(a);
        }
        Ok((agent, route.spec()))
    }
}

/// Attach the images a prompt mentions (`@shot.png`, L2.22), off the async threads.
/// Where `/skill:name` and `/template` come from; re-read on each prompt, so edits apply at once.
#[derive(Clone)]
struct Inputs {
    skills: Vec<std::path::PathBuf>,
    templates: Vec<std::path::PathBuf>,
}

impl Inputs {
    fn expand(&self, text: &str) -> String {
        if !text.starts_with('/') {
            return text.to_string();
        }
        let skills = resources::skills(&self.skills).0;
        resources::expand(text, &skills, &resources::templates(&self.templates))
    }

    /// Slash completions for the editor, and warnings about broken skills.
    fn commands(&self) -> (Vec<(String, String)>, Vec<String>) {
        let (skills, warn) = resources::skills(&self.skills);
        let mut c: Vec<(String, String)> = resources::templates(&self.templates)
            .into_iter()
            .map(|t| (format!("/{}", t.name), t.description))
            .collect();
        c.extend(skills.into_iter().map(|k| {
            let d: String = k
                .description
                .lines()
                .next()
                .unwrap_or_default()
                .chars()
                .take(80)
                .collect();
            (format!("/skill:{}", k.name), d)
        }));
        (c, warn)
    }
}

/// Expand a template or skill command, then attach mentioned images.
async fn attach(text: String, cwd: std::path::PathBuf, inputs: &Inputs) -> (Vec<novacode_types::Block>, Vec<String>) {
    let text = inputs.expand(&text);
    let fallback = text.clone();
    tokio::task::spawn_blocking(move || novacode_tools::image::attach(&text, &cwd))
        .await
        .unwrap_or_else(|_| (vec![novacode_types::Block::text(fallback)], vec![]))
}

async fn print_mode(
    agent: Agent,
    prompt: String,
    cwd: std::path::PathBuf,
    inputs: Inputs,
    verbose: bool,
    json: bool,
) -> anyhow::Result<ExitCode> {
    let state = wire::State {
        session: agent.store.id().to_string(),
        cwd: cwd.display().to_string(),
        model: format!("{}/{}", agent.config().provider, agent.config().model),
        level: agent.config().thinking,
        window_tokens: agent.config().window_tokens,
        busy: false,
        file: agent.store.path().map(|p| p.display().to_string()),
        ..Default::default()
    };
    let (handle, task) = agent::spawn(agent);
    let mut events = handle.subscribe();
    let mut out_json = json.then(std::io::stdout);
    if let Some(o) = &mut out_json {
        o.write_all(
            wire::line(&wire::ServerMsg::Hello {
                protocol: wire::PROTOCOL,
                version: env!("CARGO_PKG_VERSION").into(),
                capabilities: vec!["events".into()],
                state,
            })
            .as_bytes(),
        )?;
    }
    let on_ctrl_c = handle.clone();
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            on_ctrl_c.abort();
        }
    });
    let (blocks, notes) = attach(prompt, cwd, &inputs).await;
    for n in notes {
        eprintln!("{n}");
    }
    handle.prompt_content(blocks);
    // stdout gets only the final answer (L8.4); progress goes to stderr with -v.
    let mut last = None;
    let stop = loop {
        let ev = events.recv().await;
        if let (Some(o), Ok(e)) = (&mut out_json, &ev) {
            o.write_all(wire::line(&wire::ServerMsg::Event { event: e.clone() }).as_bytes())?;
            o.flush()?;
        }
        match ev {
            Ok(AgentEvent::Message(Message::Assistant(m))) => last = Some(m),
            Ok(AgentEvent::ToolStart { call }) if verbose => eprintln!("tool {} {}", call.name, call.arguments),
            Ok(AgentEvent::Stream(StreamEvent::Notice { text })) => eprintln!("{text}"),
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
    if !text.is_empty() && !json {
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

/// What every TUI window takes from settings.
struct TuiBase {
    keymap: Keymap,
    theme: novacode_tui::theme::Theme,
    collapse_turns: bool,
    thinking: novacode_tui::transcript::Thinking,
    notices: Vec<String>,
}

fn tui_base(paths: &Paths, settings: &Settings) -> TuiBase {
    let (keymap, mut notices) = load_keymap(paths);
    let name = settings.get::<String>("ui.theme").unwrap_or_else(|| "auto".into());
    let dark = (name == "auto").then(novacode_tui::term::query_background).flatten();
    let (mut theme, warn) = novacode_tui::theme::Theme::load(&name, &paths.home().join("themes"), dark);
    notices.extend(warn);
    if let Some(v) = settings.get::<serde_json::Value>("ui.userMessage") {
        notices.extend(theme.apply_user_message(&v));
    }
    TuiBase {
        keymap,
        theme,
        collapse_turns: settings.get("ui.turnCollapse").unwrap_or(true),
        thinking: novacode_tui::transcript::Thinking::parse(&settings.get::<String>("ui.thinking").unwrap_or_default()),
        notices,
    }
}

/// Build the agent, start its owner task and hub, and put it on its socket (L8.13).
async fn start_session(
    ctx: &Ctx,
    store: Store,
    model: &str,
    local: Option<mpsc::UnboundedSender<UiEvent>>,
    undo: &Arc<std::sync::Mutex<Vec<String>>>,
    bg: bool,
    sinks: activity::Sinks,
) -> anyhow::Result<(serve::Live, tokio::task::JoinHandle<Agent>)> {
    let slot = hub::HubSlot::default();
    let (agent, spec) = ctx.agent(store, model, Some(Arc::new(slot.clone()))).await?;
    let price = ctx
        .providers
        .resolve(&spec)
        .ok()
        .and_then(|r| models::price(&ctx.providers, &r));
    let history = agent.store.projection();
    let mut state = wire::State {
        session: agent.store.id().to_string(),
        cwd: ctx.cwd.display().to_string(),
        model: spec.clone(),
        level: agent.config().thinking,
        window_tokens: agent.config().window_tokens,
        file: agent.store.path().map(|p| p.display().to_string()),
        branch: git_branch(&ctx.cwd),
        project: activity::project(&ctx.cwd).await,
        started_ms: novacode_core::session::now_ms(),
        ..Default::default()
    };
    for m in &history {
        state.add(m);
    }
    // Priced per reply at its own model's rate (models::spent), not the current one.
    state.price = price;
    state.spent = models::spent(&ctx.providers, agent.store.entries());
    let id = agent.store.id().to_string();
    let level = state.level;
    let (handle, task) = agent::spawn(agent);
    let hub = hub::Hub::start(handle, history, state, local);
    let _ = slot.0.set(Arc::downgrade(&hub));
    let host = Arc::new(host::Host {
        hub,
        cur: Arc::new(std::sync::Mutex::new((spec, level))),
        paths: ctx.paths.clone(),
        cwd: ctx.cwd.clone(),
        id,
        inputs: ctx.inputs(),
        undo: undo.clone(),
        providers: ctx.providers.clone(),
        limits: models::Limits::new(&ctx.settings),
        trace: ctx.trace(),
    });
    let mut live = serve::go_live(&host, bg)?;
    live.add_task(activity::spawn(&host.hub, sinks));
    live.add_task(quota_task(host.clone()));
    Ok((live, task))
}

/// Keep `State.quota` current: at start and after each task (L8.15).
fn quota_task(host: Arc<host::Host>) -> tokio::task::JoinHandle<()> {
    let mut changed = host.hub.changed.subscribe();
    tokio::spawn(async move {
        let auth = AuthStore::new(host.paths.home().join("auth.json"));
        let mut was_busy = true;
        loop {
            let s = changed.borrow_and_update().clone();
            if was_busy && !s.busy {
                let (provider, model) = s.model.split_once('/').unwrap_or((&s.model, ""));
                if let Some(q) = novacode_ai::quota::cached(host.paths.home(), &auth, provider, model).await
                    && s.quota.as_ref() != Some(&q)
                {
                    host.hub.update(false, |st| st.quota = Some(q));
                }
            }
            was_busy = s.busy;
            if host.hub.is_closed() || changed.changed().await.is_err() {
                break;
            }
        }
    })
}

/// `--bg` (the child) and `--rpc`: a session with no window of its own.
async fn serve_headless(ctx: &Ctx, store: Store, model: String) -> anyhow::Result<ExitCode> {
    let undo = Arc::default();
    let (live, task) = start_session(
        ctx,
        store,
        &model,
        None,
        &undo,
        !ctx.cli.rpc,
        activity::Sinks::headless(&ctx.settings),
    )
    .await?;
    let host = live.host.clone();
    if ctx.cli.rpc {
        tokio::spawn(serve::serve(
            tokio::io::stdin(),
            tokio::io::stdout(),
            host.clone(),
            true,
        ));
    } else {
        // The parent waits for this line; nothing else goes to stdout.
        println!("{}", host.id);
        let (sink, _) = mpsc::unbounded_channel();
        if let Some(p) = ctx.cli.bg.clone().filter(|p| !p.trim().is_empty()) {
            host.handle(Cmd::Prompt(p), &sink, false).await;
        }
    }
    let on_signal = host.hub.handle.clone();
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            on_signal.abort();
            on_signal.shutdown();
        }
    });
    let agent = task.await?;
    drop(agent);
    live.end("session ended").await;
    // Let the last lines (and the bye) reach RPC clients.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    Ok(ExitCode::SUCCESS)
}

/// `/restart` (L8.10): exec the binary on disk again, resuming this session.
/// Returns only if exec failed.
fn restart(id: &str) -> anyhow::Error {
    use std::os::unix::process::CommandExt;
    let mut args = Vec::new();
    let mut skip = false;
    for a in std::env::args().skip(1) {
        if std::mem::take(&mut skip) {
            continue;
        }
        match a.as_str() {
            "-c" | "--continue" => {}
            "-r" | "--resume" => skip = true,
            s if s.starts_with("--resume=") => {}
            _ => args.push(a),
        }
    }
    args.extend(["-r".into(), id.to_string()]);
    let exe = match std::env::current_exe() {
        Ok(e) => e,
        Err(e) => return e.into(),
    };
    anyhow::anyhow!("restart failed: {}", std::process::Command::new(exe).args(args).exec())
}

/// `--bg`: re-run this command detached, wait for its session id, print it.
fn spawn_bg() -> anyhow::Result<ExitCode> {
    use std::io::BufRead;
    use std::os::unix::process::CommandExt;
    let mut child = std::process::Command::new(std::env::current_exe()?)
        .args(std::env::args_os().skip(1))
        .env("NOVACODE_BG_CHILD", "1")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        // Its own process group: closing this terminal doesn't hang it up.
        .process_group(0)
        .spawn()?;
    let mut id = String::new();
    std::io::BufReader::new(child.stdout.take().context("no pipe")?).read_line(&mut id)?;
    let id = id.trim();
    if id.is_empty() {
        let _ = child.wait();
        bail!("the background session didn't start; see ~/.novacode/logs");
    }
    eprintln!("started {id} in the background");
    eprintln!("attach with: novacode attach {}", &id[..id.len().min(13)]);
    println!("{id}");
    Ok(ExitCode::SUCCESS)
}

/// The TUI, one session at a time; /resume and /new swap the session in place.
async fn interactive_loop(mut ctx: Ctx, mut store: Store, mut model: String) -> anyhow::Result<ExitCode> {
    let mut notices = Vec::new();
    let mut draft = String::new();
    // File rewinds to undo, newest last (L4.15).
    let undo: Arc<std::sync::Mutex<Vec<String>>> = Arc::default();
    let checkpoints = ctx
        .settings
        .get("checkpoints.enabled")
        .unwrap_or(true)
        .then(|| novacode_core::checkpoint::resolve(&ctx.cwd, ctx.paths.home()))
        .flatten();
    loop {
        let (ui_tx, ui_rx) = mpsc::unbounded_channel();
        let (live, task) = start_session(
            &ctx,
            store,
            &model,
            Some(ui_tx.clone()),
            &undo,
            false,
            activity::Sinks::for_window(&ctx.settings),
        )
        .await?;
        let host = live.host.clone();
        let (handle, id) = (host.hub.handle.clone(), host.id.clone());
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
        let h = host.clone();
        tokio::spawn(async move {
            while let Some(c) = crx.recv().await {
                if c == Cmd::Quit {
                    break;
                }
                h.handle(c, &ui_tx, true).await;
            }
        });
        let (commands, warn) = ctx.inputs().commands();
        notices.extend(warn);
        let base = tui_base(&ctx.paths, &ctx.settings);
        notices.extend(base.notices);
        let state = host.hub.state();
        let init = novacode_tui::Init {
            model: state.model.clone(),
            cwd: ctx.cwd.display().to_string(),
            branch: git_branch(&ctx.cwd),
            window_tokens: state.window_tokens,
            history: host.hub.history(),
            keymap: base.keymap,
            collapse_turns: base.collapse_turns,
            level: state.level,
            thinking: base.thinking,
            theme: base.theme,
            notices: std::mem::take(&mut notices),
            price: state.price,
            spent: state.spent,
            commands,
            draft: std::mem::take(&mut draft),
            attached: false,
        };
        let res = novacode_tui::run(init, erx, ui_rx, ctx_tx).await;
        handle.abort();
        handle.shutdown();
        let agent = task.await?;
        live.end("the session's window closed").await;
        // /new and /resume keep the model; the level comes from the session (or settings).
        model = host.cur.lock().unwrap().0.clone();
        let prev = agent.store.path().map(Path::to_path_buf);
        let res = res?;
        if let Exit::Rewind { at, files, chat } = &res {
            let mut store_ = agent.store;
            if *files {
                match checkpoints.as_ref() {
                    None => notices.push("file checkpoints are off (checkpoints.enabled)".into()),
                    Some(repo) if at == "undo" => match undo.lock().unwrap().pop() {
                        Some(t) => match novacode_core::checkpoint::undo(&store_, repo, &t) {
                            Ok(n) => notices.push(format!("file rewind undone: {n} files restored")),
                            Err(e) => notices.push(format!("undo failed: {e}")),
                        },
                        None => notices.push("nothing to undo".into()),
                    },
                    Some(repo) => match novacode_core::checkpoint::rewind(&store_, repo, at) {
                        Ok(r) => {
                            undo.lock().unwrap().push(r.undo);
                            let mut m = format!("files rewound: {} changed", r.changed);
                            if !r.skipped.is_empty() {
                                m.push_str(&format!(
                                    "; left as they are (untracked, over 1 MB): {}",
                                    r.skipped.join(", ")
                                ));
                            }
                            notices.push(m);
                        }
                        Err(e) => notices.push(format!("file rewind failed: {e}")),
                    },
                }
            }
            if *chat && at != "undo" {
                let (s, err) = branch(
                    &ctx,
                    store_,
                    novacode_tui::app::BranchAction::Rewind,
                    Some(at),
                    &mut draft,
                );
                notices.extend(err);
                store_ = s;
            }
            store = store_;
            continue;
        }
        if let Exit::Branch(action, at) = &res {
            let (s, err) = branch(&ctx, agent.store, *action, at.as_deref(), &mut draft);
            store = s;
            notices.extend(err);
            continue;
        }
        if res == Exit::Reload {
            notices.extend(ctx.reload());
            notices.push("reloaded settings, keys, theme, skills and context files".into());
            store = agent.store;
            continue;
        }
        if res == Exit::Restart {
            drop(agent);
            return Err(restart(&id));
        }
        drop(agent);
        let next = match res {
            Exit::Quit => {
                if prev.as_ref().is_some_and(|p| p.exists()) {
                    eprintln!("resume with: novacode -r {}", &id[..id.len().min(13)]);
                }
                return Ok(ExitCode::SUCCESS);
            }
            Exit::New => Ok(Store::create(&ctx.paths, &ctx.cwd, ctx.cli.no_session)),
            Exit::Branch(..) | Exit::Rewind { .. } | Exit::Reload | Exit::Restart => unreachable!("handled above"),
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

/// `/tree`, `/rewind`, `/fork`, `/clone` (L4.3): move the leaf or fork a new
/// session. A taken-back prompt goes into `draft`. On error the store comes back.
fn branch(
    ctx: &Ctx,
    mut store: Store,
    action: novacode_tui::app::BranchAction,
    at: Option<&str>,
    draft: &mut String,
) -> (Store, Option<String>) {
    use novacode_tui::app::BranchAction as A;
    let Some(at) = at else {
        let Some(leaf) = store.branch().last().map(|e| e.id.clone()) else {
            return (store, Some("nothing to clone yet".into()));
        };
        return match store.fork(&ctx.paths, &leaf) {
            Ok(s) => (s, None),
            Err(e) => (store, Some(format!("clone failed: {e}"))),
        };
    };
    let Some(e) = store.entries().iter().find(|e| e.id == at).cloned() else {
        return (store, Some("that prompt is gone".into()));
    };
    let Some(parent) = e.parent.clone() else {
        return (store, Some("can't go before the start of the session".into()));
    };
    if let novacode_core::session::EntryKind::Message {
        message: novacode_types::Message::User { content },
    } = &e.kind
    {
        *draft = content
            .iter()
            .filter_map(|b| match b {
                novacode_types::Block::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
    }
    match action {
        A::Fork | A::Clone => match store.fork(&ctx.paths, &parent) {
            Ok(s) => (s, None),
            Err(e) => (store, Some(format!("fork failed: {e}"))),
        },
        A::Tree | A::Rewind => match store.set_leaf(&parent) {
            Ok(()) => (store, None),
            Err(e) => (store, Some(format!("could not move there: {e}"))),
        },
    }
}

fn export_redactor(paths: &Paths) -> Redactor {
    let auth_path = paths.home().join("auth.json");
    Redactor::new(move || AuthStore::new(auth_path.clone()).secrets())
}

/// Write a session's branch as JSONL or HTML (L4.14), secrets masked (L6.6).
fn export_session(
    paths: &Paths,
    file: &Path,
    html: bool,
    out: Option<std::path::PathBuf>,
) -> anyhow::Result<std::path::PathBuf> {
    use novacode_core::export;
    let (header, entries) = export::read(file).context("can't read that session")?;
    let r = export_redactor(paths);
    let body = if html {
        export::html(&header, &entries, &r)
    } else {
        export::jsonl(&entries, &r)
    };
    let out = out.unwrap_or_else(|| {
        std::path::PathBuf::from(format!(
            "novacode-{}.{}",
            &header.id[..header.id.len().min(13)],
            if html { "html" } else { "jsonl" }
        ))
    });
    std::fs::write(&out, body).with_context(|| format!("writing {}", out.display()))?;
    Ok(out)
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

/// `novacode login <api-key provider>`: read the key without echo, check it with a
/// free model-list call (jcode validates at login), save it in auth.json (pi).
async fn key_login(mut p: Providers, provider: &str) -> anyhow::Result<ExitCode> {
    let pr = &p.profiles[provider];
    let env = pr.env.first().cloned().unwrap_or_default();
    eprintln!("Paste your {provider} API key (or $VAR / !command to read it on each use).");
    if !env.is_empty() {
        eprintln!("Without a saved key novacode reads {env}.");
    }
    let key = read_secret("key: ")?;
    if key.is_empty() {
        bail!("no key entered; nothing saved");
    }
    p.cli_key = Some((provider.into(), key.clone()));
    let p = Arc::new(p);
    let check = tokio::time::timeout(std::time::Duration::from_secs(30), async {
        match p.adapter(provider, None).await {
            Ok((a, _)) => a.list_models().await,
            Err(e) => Err(e.to_string()),
        }
    })
    .await
    .unwrap_or_else(|_| Err("no answer in 30s".into()));
    match check {
        Ok(ids) => eprintln!("key works: {} models", ids.len()),
        Err(e) => {
            eprintln!("key check failed: {e}");
            if !std::io::stdin().is_terminal() {
                bail!("key not saved");
            }
            eprint!("save it anyway? [y/N] ");
            let mut a = String::new();
            std::io::stdin().read_line(&mut a)?;
            if !a.trim().eq_ignore_ascii_case("y") {
                bail!("key not saved");
            }
        }
    }
    p.auth.put_key(provider, &key).await?;
    eprintln!("saved in auth.json (owner-only); `novacode logout {provider}` removes it");
    Ok(ExitCode::SUCCESS)
}

/// One line from stdin with echo off when it's a terminal.
// shortcut: echo is switched with `stty`; use termios directly if a non-Unix build ever matters.
fn read_secret(prompt: &str) -> anyhow::Result<String> {
    let tty = std::io::stdin().is_terminal();
    let stty = |arg: &str| {
        let _ = std::process::Command::new("stty").arg(arg).status();
    };
    eprint!("{prompt}");
    if tty {
        stty("-echo");
    }
    let mut line = String::new();
    let r = std::io::stdin().read_line(&mut line);
    if tty {
        stty("echo");
        eprintln!();
    }
    r?;
    Ok(line.trim().to_string())
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
        _ => bail!(
            "unknown provider `{provider}` (subscriptions: claude, codex, antigravity, copilot; API keys: see `novacode models --all`)"
        ),
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
