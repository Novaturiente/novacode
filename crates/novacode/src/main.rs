//! novacode binary: CLI, print mode (L8.3, L8.4). M1: no tools, no session yet.

use anyhow::{Context, bail};
use clap::{Parser, Subcommand};
use novacode_ai::adapter::{Adapter, CacheRetention, Request, collect};
use novacode_ai::anthropic::{Anthropic, Auth, OAuthProfile};
use novacode_ai::auth::{self, AuthStore, DEFAULT_ACCOUNT, claude};
use novacode_ai::trace::Trace;
use novacode_base::{logging, new_id, paths::Paths, settings::Settings};
use novacode_types::{Message, StopReason};
use std::io::{IsTerminal, Read, Write};
use std::process::ExitCode;
use tokio_util::sync::CancellationToken;

#[derive(Parser)]
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
    /// Report stop reason and token/cache usage on stderr.
    #[arg(short, long)]
    verbose: bool,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Log in to a subscription provider (claude).
    Login { provider: String },
    /// Remove a stored login.
    Logout { provider: String },
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
    let store = AuthStore::new(paths.home().join("auth.json"));
    match cli.command {
        Some(Command::Login { provider }) => return login(&store, &provider).await,
        Some(Command::Logout { provider }) => {
            let had = store.remove(&provider).await?;
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
        None => {}
    }
    let Some(mut prompt) = cli.print else {
        eprintln!(
            "novacode {}: interactive mode arrives in M4; use -p PROMPT",
            env!("CARGO_PKG_VERSION")
        );
        return Ok(ExitCode::from(2));
    };
    let stdin = std::io::stdin();
    if !stdin.is_terminal() {
        let mut piped = String::new();
        stdin.lock().read_to_string(&mut piped)?;
        if !piped.trim().is_empty() {
            prompt = format!("{prompt}\n\n{piped}");
        }
    }

    let cwd = std::env::current_dir()?;
    // shortcut: project settings stay off until project trust lands (M3, L6.3).
    let settings = Settings::load(&paths.settings(), Some(&Paths::project_settings(&cwd)), false);
    let model = cli
        .model
        .or_else(|| settings.get::<String>("model"))
        .context("no model: pass -m provider/model or set \"model\" in settings")?;
    let (provider, model_id) = model
        .split_once('/')
        .context("model must be provider/model, e.g. claude/claude-opus-5-5")?;

    let auth = match provider {
        claude::PROVIDER => {
            let d = OAuthProfile::default();
            let get = |k: &str, def: String| settings.get::<String>(&format!("providers.claude.{k}")).unwrap_or(def);
            let profile = OAuthProfile {
                user_agent: get("userAgent", d.user_agent),
                betas: get("betas", d.betas),
                billing_header: get("billingHeader", d.billing_header),
                identity: get("identity", d.identity),
            };
            Auth::OAuth {
                access_token: store.access_token(claude::PROVIDER, DEFAULT_ACCOUNT).await?,
                profile,
            }
        }
        "anthropic" => {
            let raw = settings
                .get::<String>("providers.anthropic.apiKey")
                .unwrap_or_else(|| "$ANTHROPIC_API_KEY".into());
            Auth::ApiKey(auth::resolve_key(&raw)?)
        }
        other => bail!("unknown provider `{other}` (M1 supports claude, anthropic)"),
    };
    let trace = cli.trace.then(|| Trace {
        path: paths.logs().join(format!("trace-{}.jsonl", new_id())),
    });
    if let Some(t) = &trace {
        eprintln!("trace: {}", t.path.display());
    }
    let cache = match settings.get::<String>("cacheRetention").as_deref() {
        Some("long") => CacheRetention::Long,
        _ => CacheRetention::Short,
    };
    let req = Request {
        model: model_id.to_string(),
        messages: vec![Message::user(prompt)],
        tools: vec![],
        max_tokens: settings.get("maxTokens").unwrap_or(16_384),
        cache,
    };

    let cancel = CancellationToken::new();
    let on_ctrl_c = cancel.clone();
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            on_ctrl_c.cancel();
        }
    });
    let msg = collect(Anthropic::new(auth, trace).stream(req, cancel)).await;
    tracing::info!(stop = ?msg.stop, usage = ?msg.usage, "print mode done");
    let text = msg.text();
    if !text.is_empty() {
        let mut out = std::io::stdout().lock();
        writeln!(out, "{text}")?;
        out.flush()?;
    }
    if cli.verbose {
        let u = msg.usage;
        eprintln!(
            "stop={:?} input={} output={} cache_read={} cache_write={}",
            msg.stop, u.input, u.output, u.cache_read, u.cache_write
        );
    }
    Ok(match msg.stop {
        StopReason::Stop | StopReason::ToolUse | StopReason::Length => ExitCode::SUCCESS,
        StopReason::Aborted => ExitCode::from(130),
        StopReason::Error { kind } => {
            eprintln!("novacode: {kind:?}: {}", msg.error_message.unwrap_or_default());
            ExitCode::from(1)
        }
    })
}

async fn login(store: &AuthStore, provider: &str) -> anyhow::Result<ExitCode> {
    if provider != claude::PROVIDER {
        bail!("unknown login provider `{provider}` (M1 supports: claude)");
    }
    let (verifier, challenge) = auth::pkce();
    let url = claude::authorize_url(&verifier, &challenge);
    eprintln!("Open this URL, sign in, then paste the code shown:\n\n{url}\n");
    let _ = std::process::Command::new("xdg-open")
        .arg(&url)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
    eprint!("code: ");
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    let cred = claude::exchange(&line, &verifier).await?;
    store.put(claude::PROVIDER, DEFAULT_ACCOUNT, cred).await?;
    eprintln!("logged in to claude");
    Ok(ExitCode::SUCCESS)
}
