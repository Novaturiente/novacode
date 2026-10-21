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
mod agents;
mod diag;
mod doctor;
mod goals;
mod host;
mod hub;
mod import;
mod models;
mod schedule;
mod serve;
mod stats;
mod status;
mod update;
mod usage;
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
    /// Time each startup phase, print the table and exit before the session starts.
    #[arg(long)]
    profile_startup: bool,
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
    /// Start in git worktree <repo>/.worktrees/NAME on branch worktree-NAME, made if missing
    /// (no NAME: a new one named from the time). With -r, reopens a session that was in it.
    #[arg(short = 'w', long, value_name = "NAME", num_args = 0..=1, default_missing_value = "")]
    worktree: Option<String>,
    /// Permission mode for this session: auto, ask or read-only.
    #[arg(long, value_parser = parse_mode)]
    mode: Option<Mode>,
    /// Restrict tools to a named profile, e.g. scout.
    #[arg(long)]
    profile: Option<String>,
    /// Run as this subagent definition: its tools, role prompt and model.
    #[arg(long, hide = true)]
    agent: Option<String>,
    /// A subagent's tools, comma-separated (set by its parent; default: its definition's).
    #[arg(long, hide = true)]
    tools: Option<String>,
    /// A subagent's MCP servers, comma-separated (default: its definition's, else none).
    #[arg(long, hide = true)]
    mcp: Option<String>,
    /// Trust this folder's project config for this run only.
    #[arg(long)]
    trust: bool,
    /// Don't load AGENTS.md / CLAUDE.md context files.
    #[arg(long)]
    no_context_files: bool,
    /// With -p: write the wire protocol's JSONL events to stdout instead of the answer.
    #[arg(long, requires = "print")]
    json: bool,
    /// Serve the wire protocol on stdin/stdout: JSONL commands in, events out.
    #[arg(long, conflicts_with = "print")]
    rpc: bool,
    /// Run in the background with no window; prints the session id. Attach with `novacode attach`.
    #[arg(long, value_name = "PROMPT", num_args = 0..=1, default_missing_value = "", conflicts_with_all = ["print", "rpc"])]
    bg: Option<String>,
    /// API key for the model's provider, for this run only (ahead of saved and env keys).
    #[arg(long, value_name = "KEY")]
    api_key: Option<String>,
    /// Start the interactive session with this prompt (`novacode "fix the build"`).
    #[arg(value_name = "PROMPT")]
    initial: Option<String>,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand, Clone)]
enum AuthCmd {
    /// Each login and key: where it comes from and whether its token is valid.
    Status,
}

#[derive(Subcommand, Clone)]
enum DebugCmd {
    /// Write novacode-debug-<ms>.tar.gz: logs, crash logs, settings and the newest
    /// session here (or --session ID, or --no-session), secrets masked.
    Bundle {
        #[arg(long)]
        session: Option<String>,
        #[arg(long)]
        no_session: bool,
        #[arg(long, short)]
        out: Option<std::path::PathBuf>,
    },
    /// Panic on purpose, to check the crash log.
    #[command(hide = true)]
    Crash,
}

#[derive(Subcommand, Clone)]
enum Command {
    /// Log in to a subscription (claude, codex, antigravity, copilot), or save an
    /// API key for any other provider (openrouter, openai, google, …).
    /// Several subscription accounts (--account work) rotate when one runs out.
    Login {
        /// claude, codex, antigravity, copilot, or an API-key provider (openrouter, openai, …)
        provider: String,
        /// Account name, for several logins to one subscription (they rotate when one runs out)
        #[arg(long, default_value = DEFAULT_ACCOUNT)]
        account: String,
    },
    /// Remove a stored login (one account with --account, else all of them).
    Logout {
        /// The provider to log out of
        provider: String,
        /// Only this account (default: every account of the provider)
        #[arg(long)]
        account: Option<String>,
    },
    /// List models of connected providers (with a filter); --all lists every provider; --refresh updates the catalog.
    Models {
        /// Show only models whose provider/id contains this text
        filter: Option<String>,
        /// Every provider in the catalog, not only connected ones
        #[arg(long)]
        all: bool,
        /// Download the newest model catalog first
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
    /// The side panel of a live session, full screen (what the tmux panel pane shows)
    Panel {
        #[arg(long)]
        session: String,
    },
    /// List live sessions on this machine
    Agents,
    /// End a live session, a background one included
    Stop { session: String },
    /// Status of a live session: this pane's, else this folder's newest
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
    /// Diagnostics: `debug bundle` packs logs, settings and a session, secrets masked.
    Debug {
        #[command(subcommand)]
        what: DebugCmd,
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
    /// Install a package (a novacode package or a Claude Code plugin) from a git URL, at its latest
    /// release (pinned in ~/.novacode/packages.lock.json), or link a local folder.
    Install {
        source: String,
        #[arg(long)]
        name: Option<String>,
        /// A tag, branch or commit instead of the latest release.
        #[arg(long = "ref")]
        git_ref: Option<String>,
    },
    /// Remove an installed package, by name or by the URL/path it was installed from.
    Uninstall { name: String },
    /// Update novacode itself (release installs; AUR and source builds say how to update).
    Update,
    /// Installed packages; `packages update [name]` moves them to their latest release,
    /// `packages config <name>` sets a plugin's options,
    /// `packages install` installs what this project's .novacode/packages.json declares.
    Packages {
        #[command(subcommand)]
        what: Option<PkgCmd>,
    },
    /// MCP servers: list (connects to each), add, remove.
    Mcp {
        #[command(subcommand)]
        what: McpCmd,
    },
    /// Copy Claude Code's hooks, MCP servers, permission rules and model, once (shows the changes first).
    Import {
        #[command(subcommand)]
        what: ImportCmd,
    },
    /// Run a Claude Code hook as a novacode hook (written by `novacode import claude`).
    #[command(hide = true)]
    ClaudeHook {
        event: String,
        matcher: String,
        command: String,
    },
    /// Trust a folder's project config (default: the current folder); --revoke to distrust it.
    Trust {
        path: Option<std::path::PathBuf>,
        #[arg(long)]
        revoke: bool,
    },
    /// Print a shell function that follows the session's folder (e.g. a worktree) when
    /// novacode exits. Add to your rc: `novacode shell-init fish | source` (fish) or
    /// `eval "$(novacode shell-init zsh)"` (zsh, bash).
    ShellInit {
        #[arg(value_parser = ["fish", "zsh", "bash"])]
        shell: String,
    },
}

#[derive(clap::Subcommand, Clone)]
enum PkgCmd {
    /// Move packages (all, or one) to their latest release; a followed branch to its newest commit.
    Update {
        name: Option<String>,
        /// Also move packages without releases to the newest commit of their default branch.
        #[arg(long)]
        commits: bool,
    },
    /// Set the options a Claude Code plugin declares (its userConfig).
    Config { name: String },
    /// Install the packages this project declares, at the commits its lockfile pins.
    Install,
}

#[derive(clap::Subcommand, Clone)]
enum ImportCmd {
    /// From ~/.claude/settings.json and ~/.claude.json.
    Claude {
        /// Apply without asking.
        #[arg(long)]
        yes: bool,
    },
}

#[derive(clap::Subcommand, Clone)]
enum McpCmd {
    /// Every configured server with its status and tools.
    List,
    /// Add a stdio server to ~/.novacode/mcp.json: `novacode mcp add name -- cmd args…`, or --url for HTTP.
    Add {
        name: String,
        #[arg(long)]
        url: Option<String>,
        /// KEY=VALUE environment for the server (repeatable).
        #[arg(long = "env")]
        env: Vec<String>,
        #[arg(last = true)]
        command: Vec<String>,
    },
    /// Remove a server from ~/.novacode/mcp.json.
    Remove { name: String },
    /// Log in to an HTTP server that uses OAuth (opens the browser).
    Login { name: String },
    /// Forget a server's OAuth login.
    Logout { name: String },
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
    diag::mark("parse args");
    let _log = match logging::init(&paths, cli.trace) {
        Ok(g) => g,
        Err(e) => return fail(e.into()),
    };
    diag::install_crash_hook(&paths);
    diag::mark("logging");
    let rt = match tokio::runtime::Runtime::new() {
        Ok(rt) => rt,
        Err(e) => return fail(e.into()),
    };
    let r = rt.block_on(async {
        let r = run(cli, paths).await;
        novacode_core::hooks::flush(std::time::Duration::from_secs(5)).await;
        r
    });
    match r {
        Ok(code) => code,
        Err(e) => fail(e),
    }
}

async fn mcp_command(paths: &Paths, what: McpCmd) -> anyhow::Result<ExitCode> {
    use novacode_core::mcp;
    match what {
        McpCmd::List => {
            let cwd = std::env::current_dir()?;
            let trusted = trust::resolve(&TrustStore::new(paths.home().join("trust.json")), &cwd, false);
            let (servers, warnings) = mcp::load(paths.home(), &cwd, trusted);
            for w in warnings {
                eprintln!("warning: {w}");
            }
            if !trusted && mcp::project_file(&cwd).is_some() {
                eprintln!("note: this folder's .mcp.json is ignored (not trusted)");
            }
            if servers.is_empty() {
                println!("no MCP servers (add one with `novacode mcp add name -- command args…`)");
                return Ok(ExitCode::SUCCESS);
            }
            let sources: Vec<_> = servers.iter().map(|s| (s.name.clone(), s.source.clone())).collect();
            let hub = mcp::Hub::start(servers, paths.home(), &cwd);
            hub.settle().await;
            let tools = hub.tools();
            let mut bad = false;
            for (name, status, n, _) in hub.status() {
                let src = sources
                    .iter()
                    .find(|(s, _)| *s == name)
                    .map(|(_, p)| p.display().to_string())
                    .unwrap_or_default();
                let st = match status {
                    mcp::Status::Ready => "ready".to_string(),
                    mcp::Status::Connecting => "connecting".into(),
                    mcp::Status::Failed(e) => {
                        bad = true;
                        format!("failed: {e}")
                    }
                };
                println!("{name}  {st}  {n} tools  ({src})");
                let mine: Vec<_> = tools
                    .iter()
                    .filter(|t| t.name().starts_with(&format!("{name}__")))
                    .collect();
                for t in mine.iter().take(15) {
                    let ex = match t.exposure() {
                        novacode_tools::Exposure::Listed => "listed",
                        novacode_tools::Exposure::CodemodeOnly => "codemode-only",
                        novacode_tools::Exposure::Deferred => "deferred",
                        novacode_tools::Exposure::Hidden => "hidden",
                    };
                    println!("  {}  [{ex}]", t.name());
                }
                if mine.len() > 15 {
                    println!("  … {} more", mine.len() - 15);
                }
            }
            Ok(if bad { ExitCode::from(1) } else { ExitCode::SUCCESS })
        }
        McpCmd::Add {
            name,
            url,
            env,
            command,
        } => {
            let mut v = serde_json::Map::new();
            match (url, command.split_first()) {
                (Some(u), _) => {
                    v.insert("url".into(), u.into());
                }
                (None, Some((cmd, args))) => {
                    v.insert("command".into(), cmd.clone().into());
                    v.insert("args".into(), serde_json::json!(args));
                }
                (None, None) => anyhow::bail!("give a command after `--`, or --url"),
            }
            let env = env
                .iter()
                .map(|kv| match kv.split_once('=') {
                    Some((k, val)) if !k.is_empty() => Ok((k.to_string(), serde_json::Value::from(val))),
                    _ => Err(anyhow::anyhow!("--env needs KEY=VALUE, got `{kv}`")),
                })
                .collect::<anyhow::Result<serde_json::Map<_, _>>>()?;
            if !env.is_empty() {
                v.insert("env".into(), env.into());
            }
            mcp::edit_global(paths.home(), &name, Some(v.into()))?;
            eprintln!("added `{name}` to {}", paths.home().join("mcp.json").display());
            Ok(ExitCode::SUCCESS)
        }
        McpCmd::Login { name } => {
            let cwd = std::env::current_dir()?;
            let trusted = trust::resolve(&TrustStore::new(paths.home().join("trust.json")), &cwd, false);
            let (servers, _) = mcp::load(paths.home(), &cwd, trusted);
            let Some(s) = servers.into_iter().find(|s| s.name == name) else {
                bail!("no MCP server `{name}` (see `novacode mcp list`)");
            };
            let Some(url) = s.config.url.as_deref().map(mcp::expand) else {
                bail!("`{name}` is a local (stdio) server; only HTTP servers use OAuth");
            };
            let listener =
                tokio::net::TcpListener::bind(("127.0.0.1", s.config.oauth.callback_port.unwrap_or(0))).await?;
            let redirect = format!("http://127.0.0.1:{}/callback", listener.local_addr()?.port());
            let pending = novacode_core::mcp_oauth::begin(&url, &s.config.oauth, &redirect)
                .await
                .map_err(anyhow::Error::msg)?;
            eprintln!(
                "Open this URL and approve novacode:\n\n{}\n\nnovacode catches the redirect on {redirect}. On another machine, paste the URL the browser ends on:",
                pending.url
            );
            let _ = std::process::Command::new("xdg-open")
                .arg(&pending.url)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn();
            let (code, state) = catch_or_paste(Some(listener)).await?;
            if state.as_deref() != Some(pending.state.as_str()) {
                bail!("OAuth state mismatch; start the login again");
            }
            novacode_core::mcp_oauth::finish(paths.home(), &name, pending, &code)
                .await
                .map_err(anyhow::Error::msg)?;
            eprintln!("logged in to `{name}`; new sessions use it");
            Ok(ExitCode::SUCCESS)
        }
        McpCmd::Logout { name } => {
            if novacode_core::mcp_oauth::remove(paths.home(), &name)
                .await
                .map_err(anyhow::Error::msg)?
            {
                eprintln!("logged out of `{name}`");
            } else {
                eprintln!("no login saved for `{name}`");
            }
            Ok(ExitCode::SUCCESS)
        }
        McpCmd::Remove { name } => {
            if mcp::edit_global(paths.home(), &name, None)? {
                eprintln!("removed `{name}`");
                Ok(ExitCode::SUCCESS)
            } else {
                eprintln!("no server `{name}` in {}", paths.home().join("mcp.json").display());
                Ok(ExitCode::from(1))
            }
        }
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
                let dirs = |k, c| resource_dirs(&paths, &cwd, &settings, trusted, k, c);
                let mut shadowed = novacode_core::resources::skills(&dirs("skills", "skills")).2;
                shadowed.extend(novacode_core::resources::templates(&dirs("prompts", "commands")).1);
                shadowed.extend(agents::defs(&dirs("agents", "agents")).1);
                shadowed.extend(novacode_core::packages::problems(paths.home(), &cwd, trusted));
                for pkg in novacode_core::packages::enabled(paths.home(), &cwd, trusted) {
                    for s in novacode_core::plugin::Parts::of(&pkg).hooks().1 {
                        shadowed.push(format!("package `{}`: {s} is skipped", pkg.name));
                    }
                }
                return Ok(doctor::doctor(&p, &settings, problems, shadowed).await);
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
            return serve::attach_loop(&paths, &session, || tui_base(&paths, &settings), false).await;
        }
        Some(Command::Panel { session }) => {
            let cwd = std::env::current_dir()?;
            let trusted = trust::resolve(&TrustStore::new(paths.home().join("trust.json")), &cwd, false);
            let settings = Settings::load(&paths.settings(), Some(&Paths::project_settings(&cwd)), trusted);
            return serve::attach_loop(&paths, &session, || tui_base(&paths, &settings), true).await;
        }
        Some(Command::Debug {
            what:
                DebugCmd::Bundle {
                    session,
                    no_session,
                    out,
                },
        }) => {
            let cwd = std::env::current_dir()?;
            let file = match (session, no_session) {
                (_, true) => None,
                (Some(p), _) => Some(session::find(&paths, &cwd, &p)?),
                (None, _) => session::list(&paths, &cwd).into_iter().next(),
            };
            let out = diag::bundle(&paths, &cwd, file.as_deref(), out)?;
            println!("{}", out.display());
            return Ok(ExitCode::SUCCESS);
        }
        Some(Command::Debug { what: DebugCmd::Crash }) => panic!("test crash (novacode debug crash)"),
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
        Some(Command::Mcp { what }) => return mcp_command(&paths, what).await,
        Some(Command::ClaudeHook {
            event,
            matcher,
            command,
        }) => {
            return Ok(import::hook(&event, &matcher, &command));
        }
        Some(Command::Import {
            what: ImportCmd::Claude { yes },
        }) => {
            let settings = Settings::load(&paths.settings(), None, false);
            return import::run(&paths, &Providers::load(paths.home(), &settings), yes);
        }
        Some(Command::Update) => return update::run().await,
        Some(Command::Install { source, name, git_ref }) => {
            use novacode_core::packages::{Store, Want};
            let store = Store::global(paths.home());
            let want = git_ref.as_deref().map_or(Want::Latest, Want::Ref);
            let n = store
                .install(&source, name.as_deref(), want)
                .await
                .map_err(anyhow::Error::msg)?;
            let at = store
                .pin(&n)
                .map(|p| format!(" at {}", novacode_core::packages::version_of(&p)))
                .unwrap_or_default();
            eprintln!("installed `{n}`{at}; new sessions load it");
            if let Some(p) = store.list().into_iter().find(|p| p.name == n) {
                let parts = novacode_core::plugin::Parts::of(&p);
                eprintln!("{}", package_summary(&parts));
                ask_options(&parts, true)?;
            }
            return Ok(ExitCode::SUCCESS);
        }
        Some(Command::Packages {
            what: Some(PkgCmd::Config { name }),
        }) => {
            let p = novacode_core::packages::Store::global(paths.home())
                .list()
                .into_iter()
                .find(|p| p.name == name)
                .ok_or_else(|| anyhow::anyhow!("`{name}` is not installed"))?;
            let parts = novacode_core::plugin::Parts::of(&p);
            if parts.option_specs().is_empty() {
                eprintln!("`{name}` has no options");
            } else {
                ask_options(&parts, false)?;
            }
            return Ok(ExitCode::SUCCESS);
        }
        Some(Command::Uninstall { name }) => {
            let name = novacode_core::packages::Store::global(paths.home())
                .uninstall(&name)
                .map_err(anyhow::Error::msg)?;
            eprintln!("removed `{name}`");
            return Ok(ExitCode::SUCCESS);
        }
        Some(Command::Packages {
            what: Some(PkgCmd::Update { name, commits }),
        }) => {
            let mut bad = false;
            let store = novacode_core::packages::Store::global(paths.home());
            for (n, r) in store.update(name.as_deref(), commits).await {
                match r {
                    Ok(msg) => eprintln!("{n}: {msg}"),
                    Err(e) => {
                        bad = true;
                        eprintln!("{n}: {e}");
                    }
                }
            }
            return Ok(if bad { ExitCode::from(1) } else { ExitCode::SUCCESS });
        }
        Some(Command::Packages {
            what: Some(PkgCmd::Install),
        }) => {
            let cwd = std::env::current_dir()?;
            if !trust::resolve(&TrustStore::new(paths.home().join("trust.json")), &cwd, false) {
                bail!("this folder isn't trusted; run `novacode trust` first");
            }
            let r = novacode_core::packages::install_project(&cwd).await;
            if r.is_empty() {
                eprintln!("nothing to install");
            }
            let mut bad = false;
            for (n, r) in r {
                match r {
                    Ok(_) => eprintln!("installed `{n}`"),
                    Err(e) => {
                        bad = true;
                        eprintln!("{n}: {e}");
                    }
                }
            }
            return Ok(if bad { ExitCode::from(1) } else { ExitCode::SUCCESS });
        }
        Some(Command::Packages { what: None }) => {
            let cwd = std::env::current_dir()?;
            let mut stores = vec![("", novacode_core::packages::Store::global(paths.home()))];
            if cwd.join(".novacode/packages").is_dir() {
                stores.push(("project ", novacode_core::packages::Store::project(&cwd)));
            }
            let mut none = true;
            for (label, store) in stores {
                for p in store.list() {
                    none = false;
                    let parts = novacode_core::plugin::Parts::of(&p);
                    let src = match (&p.link, store.pin(&p.name)) {
                        (Some(l), _) => format!("→ {}", l.display()),
                        (None, Some(pin)) => format!("git {}", novacode_core::packages::version_of(&pin)),
                        (None, None) => "git".into(),
                    };
                    let off = p.disabled.map(|w| format!("  (disabled: {w})")).unwrap_or_default();
                    println!("{label}{}  {src}{off}", p.name);
                    println!("  {}", package_summary(&parts).replace('\n', "\n  "));
                }
            }
            if none {
                println!("no packages (install one with `novacode install <git url | folder>`)");
            }
            return Ok(ExitCode::SUCCESS);
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
        Some(Command::ShellInit { shell }) => {
            print!("{}", if shell == "fish" { SHELL_FISH } else { SHELL_SH });
            return Ok(ExitCode::SUCCESS);
        }
        None => {}
    }
    // --bg: start a detached copy of this command and report its session id.
    if cli.bg.is_some() && std::env::var_os("NOVACODE_BG_CHILD").is_none() {
        return spawn_bg();
    }
    let headless = cli.bg.is_some() || cli.rpc || cli.profile_startup;
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
        if p.trim().is_empty() {
            bail!("-p needs a prompt (an argument or piped stdin)");
        }
    } else if !stdin.is_terminal() || !std::io::stdout().is_terminal() {
        bail!("interactive mode needs a terminal; use -p PROMPT for pipes and scripts");
    }
    let interactive = prompt.is_none() && !headless;

    // A resumed session continues in the folder it was last in (worktree moves
    // included), so trust, settings and the shell wrapper follow it.
    let here = std::env::current_dir()?;
    let resumed = if let Some(prefix) = &cli.resume {
        Some(Store::open(&session::find(&paths, &here, prefix)?)?)
    } else if cli.r#continue {
        let last = session::list(&paths, &here)
            .into_iter()
            .next()
            .context("no earlier session in this folder")?;
        Some(Store::open(&last)?)
    } else {
        None
    };
    if let Some(dir) = resumed.as_ref().map(Store::last_cwd)
        && dir != here
        && dir.is_dir()
    {
        std::env::set_current_dir(&dir)?;
        eprintln!("resuming in {}", dir.display());
    }
    if let Some(name) = &cli.worktree {
        let name = if name.is_empty() { chrono_name() } else { name.clone() };
        let (path, msg) =
            novacode_tools::worktree::create(&std::env::current_dir()?, &name).map_err(anyhow::Error::msg)?;
        std::env::set_current_dir(&path)?;
        eprintln!("{msg}");
    }
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
    if trusted {
        project_packages(&paths, &cwd, interactive).await?;
    }
    let settings = Settings::load(&paths.settings(), Some(&Paths::project_settings(&cwd)), trusted);
    diag::mark("trust + settings");
    let mut providers = Providers::load(paths.home(), &settings);
    diag::mark("providers + catalog");
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
        goals: Default::default(),
        sup: Default::default(),
    };
    let store = match resumed {
        Some(s) => s,
        None => Store::create(&ctx.paths, &ctx.cwd, ctx.cli.no_session),
    };
    diag::mark("open session");
    if ctx.cli.profile_startup {
        let (agent, _) = ctx.agent(store, &model, None, None, None).await?;
        diag::mark("agent ready");
        drop(agent);
        eprint!("{}", diag::profile());
        eprintln!("  (MCP servers keep connecting in the background; the first frame follows at once)");
        return Ok(ExitCode::SUCCESS);
    }
    if headless {
        return serve_headless(&ctx, store, model).await;
    }
    match prompt {
        None => interactive_loop(ctx, store, model).await,
        Some(prompt) => {
            // Nobody to approve: asks become refusals, risky commands need a justification.
            // A hub with no windows still carries subagents (L10.1): they report back by waking the run.
            let slot = hub::HubSlot::default();
            let (agent, _) = ctx.agent(store, &model, None, None, Some(slot.clone())).await?;
            let sup = ctx.sup.lock().unwrap().take();
            print_mode(
                agent,
                slot,
                sup,
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
    /// Project config loads: gates project skills.
    trusted: bool,
    /// The goals of the agent just built, for its Host (`agent()` returns only the agent).
    goals: std::sync::Mutex<Option<Arc<goals::Goals>>>,
    /// The subagent supervisor of the last agent built (print mode waits for its children).
    sup: std::sync::Mutex<Option<Arc<agents::Supervisor>>>,
}

impl Ctx {
    /// `/reload`: read trust, settings and provider config again. CLI flags stay.
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

    fn resource_dirs(&self, kind: &str, claude: &str) -> Vec<std::path::PathBuf> {
        resource_dirs(&self.paths, &self.cwd, &self.settings, self.trusted, kind, claude)
    }

    fn skill_dirs(&self) -> Vec<std::path::PathBuf> {
        self.resource_dirs("skills", "skills")
    }

    /// Agent definitions with the user's `agents.<name>` settings applied.
    fn agent_defs(&self) -> Vec<agents::Def> {
        let mut d = agents::defs(&self.resource_dirs("agents", "agents")).0;
        agents::configure(&mut d, |name| self.settings.get(&format!("agents.{name}")));
        d
    }

    fn template_dirs(&self) -> Vec<std::path::PathBuf> {
        self.resource_dirs("prompts", "commands")
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

    /// The observer's model: `memory.model`, else the provider's small
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
        asker: Option<Arc<dyn novacode_tools::ask::Asker>>,
        slot: Option<hub::HubSlot>,
    ) -> anyhow::Result<(Agent, String)> {
        // A resumed session keeps its own model unless -m says otherwise.
        let resumed = match (&self.cli.model, store.last_model()) {
            (None, Some((p, m))) => Some(format!("{p}/{m}")),
            _ => None,
        };
        // …and falls back to the given one if that model can't be resolved any more.
        let route = match resumed.as_deref().map(|r| self.providers.resolve(r)) {
            Some(Ok(r)) => r,
            _ => self.providers.resolve(model)?,
        };
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
        diag::mark("connect provider");
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
        // A subagent: its role prompt, and only its tools and MCP servers (L10.1).
        let defs = self.agent_defs();
        let def = match &self.cli.agent {
            Some(name) => Some(
                defs.iter()
                    .find(|d| &d.name == name)
                    .cloned()
                    .with_context(|| format!("no agent definition `{name}`"))?,
            ),
            None => None,
        };
        let csv = |s: &String| -> Vec<String> {
            s.split(',')
                .map(str::trim)
                .filter(|t| !t.is_empty())
                .map(String::from)
                .collect()
        };
        let allowed: Option<Vec<String>> = self.cli.tools.as_ref().map(csv).or_else(|| def.as_ref()?.tools.clone());
        let granted: Option<Vec<String>> = def
            .as_ref()
            .map(|d| self.cli.mcp.as_ref().map_or_else(|| d.mcp.clone(), csv));
        let (window_tokens, max_tokens) = models::Limits::new(s).of(&route);
        // Cross-session memory (L10.4); `memory.store: false` turns it off.
        let memories = s
            .get::<bool>("memory.store")
            .unwrap_or(true)
            .then(|| novacode_core::memstore::Memories::open(self.paths.home(), &self.cwd));
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
                agent: def
                    .as_ref()
                    .filter(|d| !d.prompt.is_empty())
                    .map(|d| (d.prompt.clone(), d.replace)),
                mode: None,
                memory_index: memories.as_ref().map(|m| {
                    m.index(
                        s.get("memory.indexItems").unwrap_or(15),
                        s.get("memory.indexChars").unwrap_or(2000),
                    )
                }),
            },
            redactor: {
                let auth_path = self.paths.home().join("auth.json");
                Redactor::new(move || {
                    let mut s = AuthStore::new(auth_path.clone()).secrets();
                    s.extend(api_key.clone().map(|k| (k, "API key".to_string())));
                    s
                })
                .with_patterns(s.get("security.maskSecrets").unwrap_or(false))
            },
        };
        let memory_on = s.get("memory.enabled").unwrap_or(true);
        let mut all = novacode_tools::builtins(false);
        all.push(Arc::new(novacode_tools::process::ProcessTool));
        all.push(Arc::new(novacode_core::web::WebFetch));
        // Codemode-only: scripts call it as models.classify().
        all.push(Arc::new(novacode_core::classifier::Classify::new(
            self.providers.clone(),
        )));
        all.push(Arc::new(novacode_core::web::WebSearch {
            auth: Arc::new(AuthStore::new(self.paths.home().join("auth.json"))),
            providers: self.providers.clone(),
            searxng: s.get::<String>("web.searxngUrl"),
        }));
        all.push(Arc::new(novacode_tools::worktree::EnterWorktree));
        all.push(Arc::new(novacode_tools::worktree::ExitWorktree(asker.clone())));
        if let Some(a) = asker.clone() {
            all.push(Arc::new(novacode_tools::ask::AskUser(a)));
        }
        all.push(Arc::new(novacode_tools::todo::Todo::from_history(&store.projection())));
        if store.path().is_some() {
            all.push(Arc::new(novacode_core::export::SessionSearch {
                sessions_root: self.paths.home().join("sessions"),
                project_dir: session::dir_for(&self.paths, &self.cwd),
                current: store.id().to_string(),
            }));
        }
        // Subagents (L10.1): not below the depth limit, and only with windows/RPC to report to.
        let place = agents::Place::from_env();
        // `schedule` (L10.5): fires into a live session, so only with windows/RPC.
        if let Some(slot) = slot.as_ref().filter(|_| approver.is_some()) {
            let sched = schedule::Schedule::from_history(&store.projection());
            sched.run(slot.clone());
            all.push(Arc::new(schedule::ScheduleTool(sched)));
        }
        // MCP servers (L5.12): a subagent starts only the ones granted to it (L10.1).
        let (mut servers, mcp_warnings) = novacode_core::mcp::load(self.paths.home(), &self.cwd, self.trusted);
        if let Some(g) = &granted {
            servers.retain(|c| g.contains(&c.name));
        }
        let server_names: Vec<String> = servers.iter().map(|c| c.name.clone()).collect();
        let mut supervisor = None;
        if let Some(slot) = slot
            && place.depth < agents::MAX_DEPTH
        {
            let small = self.providers.profile(&route.provider).ok().and_then(|p| {
                p.extra
                    .get("smallModel")?
                    .as_str()
                    .map(|m| format!("{}/{m}", route.provider))
            });
            let sup = agents::Supervisor::new(agents::Setup {
                slot,
                me: store.id().to_string(),
                home: self.paths.home().to_path_buf(),
                cwd: self.cwd.clone(),
                trusted: self.trusted,
                defs: defs.clone(),
                parent_picks: s.get("agents.parentPicksTools").unwrap_or(true),
                servers: server_names.clone(),
                stall_min: s.get("agents.stallMinutes").unwrap_or(10),
                timeout_min: s.get("agents.timeoutMinutes").unwrap_or(45),
                small_model: small,
                exe: None,
                nerd: s.get::<String>("ui.icons").as_deref() != Some("unicode"),
                unattended: approver.is_none(),
            });
            *self.sup.lock().unwrap() = Some(sup.clone());
            // Goals (/goal): audited by the `auditor` subagent.
            let g = goals::Goals::new(
                self.paths.home().join("goals").join(format!("{}.json", store.id())),
                Some(sup.clone()),
                s.get("goal.maxRuns").unwrap_or(30),
                s.get("goal.audit").unwrap_or(true),
                s.get("goal.auditModel"),
            );
            all.push(Arc::new(goals::GoalTool(g.clone())));
            *self.goals.lock().unwrap() = Some(g);
            supervisor = Some(sup.clone());
            all.push(Arc::new(agents::AgentTool::new(sup)));
        }
        // Mode presets and plan mode (L10.7).
        let policy = Arc::new(policy);
        let modes = novacode_core::modes::Modes::new(policy.clone(), s.get("modes").unwrap_or_default());
        if let Some(a) = asker.clone() {
            all.push(Arc::new(novacode_core::modes::EnterPlanMode {
                modes: modes.clone(),
                asker: a.clone(),
            }));
            all.push(Arc::new(novacode_core::modes::PlanComplete {
                modes: modes.clone(),
                asker: a,
            }));
        }
        if let Some(m) = &memories {
            all.push(Arc::new(novacode_core::memstore::MemoryTool {
                mem: m.clone(),
                session: store.id().to_string(),
            }));
        }
        if memory_on && let Some(file) = store.path() {
            all.push(Arc::new(novacode_core::memory::Recall {
                file: file.to_path_buf(),
            }));
        }
        // Script extensions (L7.2): project ones only when trusted; the project wins a name clash.
        let mut ext_dirs = Vec::new();
        if self.trusted {
            ext_dirs.push(self.cwd.join(".novacode").join("extensions"));
        }
        ext_dirs.push(self.paths.home().join("extensions"));
        for p in novacode_core::packages::enabled(self.paths.home(), &self.cwd, self.trusted) {
            ext_dirs.extend(novacode_core::plugin::Parts::of(&p).dirs("extensions"));
        }
        let procs: Vec<(String, novacode_core::process_ext::Config)> = s
            .get::<std::collections::BTreeMap<String, novacode_core::process_ext::Config>>("processExtensions")
            .unwrap_or_default()
            .into_iter()
            .collect();
        let exts = novacode_core::extensions::Extensions::load_all(
            &ext_dirs,
            &procs,
            &self.cwd,
            &std::env::var_os("HOME")
                .map(std::path::PathBuf::from)
                .unwrap_or_default(),
            &self.paths.home().join("cache").join("extensions"),
            store.id(),
            &self.paths.home().join("logs"),
            s.merged().clone(),
        )
        .await;
        diag::mark("tools + extensions");
        if let Some(a) = &asker {
            exts.set_asker(a.clone());
        }
        all.extend(exts.tools());
        let described: Vec<(String, String)> = servers
            .iter()
            .filter(|s| s.config.exposure != Some(novacode_tools::Exposure::Hidden))
            .map(|s| (s.name.clone(), s.config.description.clone().unwrap_or_default()))
            .collect();
        all.push(Arc::new(novacode_core::codemode::Codemode::new(&described)));
        for w in &mcp_warnings {
            tracing::warn!("mcp config: {w}");
        }
        let mcp = (!servers.is_empty()).then(|| novacode_core::mcp::Hub::start(servers, self.paths.home(), &self.cwd));
        if let Some(h) = &mcp {
            all.extend(h.tools());
        }
        let is_mcp = |n: &str| {
            server_names
                .iter()
                .any(|s| n.strip_prefix(s.as_str()).is_some_and(|r| r.starts_with("__")))
        };
        // A subagent is built with only its tools (L10.1), so scripts and `call` can't reach others.
        if let Some(allow) = &allowed {
            all.retain(|t| allow.iter().any(|a| a == t.name()) || is_mcp(t.name()));
        }
        let novacode_tools::Exposed {
            listed: mut tools,
            deferred,
            scripted,
        } = novacode_tools::exposed(all, |name| self.settings.get(&format!("tools.{name}.exposure")));
        if let Some(sup) = &supervisor {
            let own = tools
                .iter()
                .chain(&deferred)
                .chain(&scripted)
                .map(|t| t.name().to_string());
            sup.set_own_tools(own.filter(|n| !is_mcp(n)).collect());
        }
        // Deferred tools (L5.17): found by tool_search, run in-line once announced or through `call`.
        if !deferred.is_empty() {
            let defs = deferred
                .iter()
                .map(|t| novacode_core::pipeline::def(t.as_ref()))
                .collect();
            tools.push(Arc::new(novacode_tools::toolsearch::ToolSearch::new(defs)));
            tools.push(Arc::new(novacode_tools::toolsearch::Call));
        }
        let window = cfg.window_tokens;
        let worker = memory_on.then(|| self.memory_worker(&route, adapter.clone()));
        diag::mark("mcp config + tool list");
        let mut agent = Agent::new(adapter, tools, store, cfg, Arc::new(LocalEnv))
            .with_shared_policy(policy)
            .with_modes(modes)
            .with_quirks(self.quirks.clone())
            .with_deferred(deferred)
            .with_scripted(scripted);
        if let Some(h) = mcp {
            agent = agent.with_mcp(h);
        }
        // Command hooks (L7.1), then extension handlers; none inside a hook's own novacode (recursion guard).
        // File claims first (L10.1): two agents of one tree never edit the same file.
        let mut handlers: Vec<Arc<dyn novacode_core::hooks::Handler>> = vec![
            Arc::new(agents::Claims::new(
                &serve::run_dir(&self.paths),
                agent.store.id(),
                place,
                &self.cwd,
            )),
            Arc::new(novacode_core::hooks::BashWrites),
        ];
        if !novacode_core::hooks::disabled() {
            let hook_cfg: novacode_core::hooks::Config = s.get("hooks").unwrap_or_default();
            let cmds = Arc::new(novacode_core::hooks::Commands {
                config: hook_cfg,
                cwd: self.cwd.clone(),
                session: agent.store.id().to_string(),
            });
            novacode_core::hooks::spawn_observers(cmds.clone(), agent.subscribe());
            if !cmds.config.before_tool.is_empty() || !cmds.config.after_tool.is_empty() {
                handlers.push(cmds);
            }
            // Claude Code plugin hooks (L7.14).
            let plugin = novacode_core::plugin_hooks::PluginHooks::load(
                &novacode_core::packages::enabled(self.paths.home(), &self.cwd, self.trusted),
                &self.cwd,
                agent.store.id(),
                self.cli.agent.clone(),
            );
            if !plugin.is_empty() {
                let plugin = Arc::new(plugin);
                if plugin.has("PreToolUse") || plugin.has("PostToolUse") {
                    handlers.push(plugin.clone());
                }
                agent = agent.with_plugin_hooks(plugin);
            }
        }
        if !exts.is_empty() || !exts.failed.is_empty() {
            handlers.push(exts.clone());
            agent = agent.with_extensions(exts);
        }
        if !handlers.is_empty() {
            agent = agent.with_hooks(Arc::new(novacode_core::hooks::Hooks { handlers }));
        }
        if s.get("checkpoints.enabled").unwrap_or(true)
            && agent.store.path().is_some()
            && let Some(repo) = novacode_core::checkpoint::resolve(&self.cwd, self.paths.home())
        {
            agent = agent.with_checkpoints(repo);
        }
        let pm = novacode_tools::process::ProcessManager::new(&self.paths.home().join("processes"), agent.store.id());
        agent = agent.with_processes(pm);
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
            if let Some(m) = memories {
                agent = agent.with_memory_store(m);
            }
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
        resources::expand(text, &skills, &resources::templates(&self.templates).0)
    }

    /// Slash completions for the editor, and warnings about broken skills.
    fn commands(&self) -> (Vec<(String, String)>, Vec<String>) {
        let (skills, warn, _) = resources::skills(&self.skills);
        let mut c: Vec<(String, String)> = resources::templates(&self.templates)
            .0
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

#[allow(clippy::too_many_arguments)]
async fn print_mode(
    agent: Agent,
    slot: hub::HubSlot,
    sup: Option<Arc<agents::Supervisor>>,
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
    let exts = agent.extensions().cloned();
    let (handle, task) = agent::spawn(agent);
    let mut events = handle.subscribe();
    let hub = hub::Hub::start(handle.clone(), Vec::new(), state.clone(), None);
    hub.unattended.store(true, std::sync::atomic::Ordering::Relaxed);
    let _ = slot.0.set(Arc::downgrade(&hub));
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
    let prompt = match exts {
        Some(e) => match e.input(&prompt, false).await {
            Some(p) => p,
            None => {
                eprintln!("(an extension handled the prompt)");
                handle.shutdown();
                let _ = task.await;
                return Ok(ExitCode::SUCCESS);
            }
        },
        None => prompt,
    };
    let (blocks, notes) = attach(prompt, cwd, &inputs).await;
    for n in notes {
        eprintln!("{n}");
    }
    handle.prompt_content(blocks);
    // stdout gets only the final answer (L8.4); progress goes to stderr with -v.
    let mut last = None;
    let mut end = None;
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
            Ok(AgentEvent::AgentEnd { stop }) => end = Some(stop),
            // Done when nothing is queued and no subagent is still out: its report would start another run.
            Ok(AgentEvent::Settled) if end.is_some() && sup.as_ref().is_none_or(|s| s.pending() == 0) => {
                break end.take().unwrap_or(StopReason::Stop);
            }
            Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {}
            Err(broadcast::error::RecvError::Closed) => bail!("session ended unexpectedly"),
        }
    };
    drop(hub);
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
        StopReason::Error { .. } => {
            eprintln!(
                "novacode: {}",
                msg.error_message.unwrap_or_else(|| "request failed".into())
            );
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
pub(crate) struct TuiBase {
    keymap: Keymap,
    theme: novacode_tui::theme::Theme,
    collapse_turns: bool,
    thinking: novacode_tui::transcript::Thinking,
    notices: Vec<String>,
    statusline: novacode_tui::statusline::Config,
    panel_auto: bool,
    nerd_icons: bool,
    slow: bool,
    images: String,
}

/// `ui.panel.host`: `tmux` (or `auto` inside tmux) opens the panel as a split
/// pane running `novacode panel --session <id>`. Returns the pane id to close at the end.
fn tmux_panel(settings: &Settings, session: &str, home: &Path) -> Option<String> {
    let host = settings
        .get::<String>("ui.panel.host")
        .unwrap_or_else(|| "inline".into());
    let in_tmux = std::env::var_os("TMUX").is_some();
    if !(host == "tmux" || host == "auto" && in_tmux) || !in_tmux {
        return None;
    }
    let exe = std::env::current_exe().ok()?;
    let width = settings
        .get::<String>("ui.panel.tmuxWidth")
        .unwrap_or_else(|| "30%".into());
    let cmd = format!("'{}' panel --session {session}", exe.display());
    // A new pane gets tmux's environment, not ours: pass the home along.
    let env = format!("NOVACODE_HOME={}", home.display());
    let out = std::process::Command::new("tmux")
        .args([
            "split-window",
            "-h",
            "-d",
            "-l",
            &width,
            "-e",
            &env,
            "-P",
            "-F",
            "#{pane_id}",
            &cmd,
        ])
        .output()
        .ok()?;
    let pane = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if !out.status.success() || pane.is_empty() {
        tracing::warn!("tmux panel pane failed: {}", String::from_utf8_lossy(&out.stderr));
        return None;
    }
    // `@slot` makes the user's tmux state save skip it.
    let _ = std::process::Command::new("tmux")
        .args(["set-option", "-p", "-t", &pane, "@slot", "panel"])
        .status();
    Some(pane)
}

/// L9.17: over SSH, or when the 1-minute load is above the core count.
fn slow_terminal() -> bool {
    let ssh = std::env::var_os("SSH_CONNECTION").is_some() || std::env::var_os("SSH_TTY").is_some();
    let load = std::fs::read_to_string("/proc/loadavg")
        .ok()
        .and_then(|s| s.split_whitespace().next()?.parse::<f64>().ok())
        .unwrap_or(0.0);
    let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1) as f64;
    ssh || load > cores
}

fn tui_base(paths: &Paths, settings: &Settings) -> TuiBase {
    let (keymap, mut notices) = load_keymap(paths);
    let name = settings.get::<String>("ui.theme").unwrap_or_else(|| "auto".into());
    let dark = (name == "auto").then(novacode_tui::term::query_background).flatten();
    // Your themes first, then installed packages' (L7.7).
    let mut dir = paths.home().join("themes");
    if !dir.join(format!("{name}.json")).is_file()
        && let Some(p) = novacode_core::packages::Store::global(paths.home())
            .list()
            .into_iter()
            .find(|p| p.disabled.is_none() && p.dir.join("themes").join(format!("{name}.json")).is_file())
    {
        dir = p.dir.join("themes");
    }
    let (mut theme, warn) = novacode_tui::theme::Theme::load(&name, &dir, dark);
    notices.extend(warn);
    if let Some(v) = settings.get::<serde_json::Value>("ui.userMessage") {
        notices.extend(theme.apply_user_message(&v));
    }
    let (statusline, warn) =
        novacode_tui::statusline::Config::parse(settings.get::<serde_json::Value>("ui.statusline").as_ref());
    notices.extend(warn);
    TuiBase {
        keymap,
        theme,
        collapse_turns: settings.get("ui.turnCollapse").unwrap_or(true),
        thinking: novacode_tui::transcript::Thinking::parse(&settings.get::<String>("ui.thinking").unwrap_or_default()),
        notices,
        statusline,
        panel_auto: settings.get("ui.panel.auto").unwrap_or(true),
        nerd_icons: settings.get::<String>("ui.icons").is_none_or(|v| v != "unicode"),
        slow: settings.get::<bool>("ui.slow").unwrap_or_else(slow_terminal),
        images: settings.get::<String>("ui.images").unwrap_or_else(|| "auto".into()),
    }
}

/// Build the agent, start its owner task and hub, and put it on its socket.
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
    let (agent, spec) = ctx
        .agent(
            store,
            model,
            Some(Arc::new(slot.clone())),
            Some(Arc::new(slot.clone())),
            Some(slot.clone()),
        )
        .await?;
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
    let processes = agent.processes().cloned();
    let mcp = agent.mcp().cloned();
    let extensions = agent.extensions().cloned();
    let modes = agent.modes().cloned();
    let goals = ctx.goals.lock().unwrap().take();
    let (handle, task) = agent::spawn(agent);
    let hub = hub::Hub::start(handle, history, state, local);
    if let Some(e) = &extensions {
        hub.seed_ui(e.setup_ui());
    }
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
        processes,
        mcp,
        extensions,
        modes,
        goals: goals.clone(),
    });
    let mut live = serve::go_live(&host, bg)?;
    if let Some(g) = goals {
        live.add_task(g.watch(host.hub.handle.clone()));
    }
    live.add_task(activity::spawn(&host.hub, sinks));
    live.add_task(quota_task(host.clone()));
    if let Some(m) = host.mcp.clone() {
        let h = host.clone();
        live.add_task(tokio::spawn(async move {
            // MCP connection counts for the `mcp` segment (pi's mcp-status had to poll too).
            loop {
                let st = m.status();
                let ready = st.iter().filter(|s| s.1 == novacode_core::mcp::Status::Ready).count();
                let now = Some((ready, st.len()));
                if h.hub.state().mcp != now {
                    h.hub.update(false, |s| s.mcp = now);
                }
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            }
        }));
    }
    Ok((live, task))
}

/// Keep `State.quota` current: at start and after each task.
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

/// Folders for one resource kind, first wins: project `.novacode`, project
/// `.claude`, `~/.novacode`, `~/.claude`, `~/.agents` (skills only), then packages.
/// `.claude` folders unless `compat.claude` is false; project folders when trusted
/// (prompt templates are plain text: the project's own `prompts` loads either way).
/// `claude` is the folder's name under `.claude` (`commands` for templates).
fn resource_dirs(
    paths: &Paths,
    cwd: &Path,
    settings: &Settings,
    trusted: bool,
    kind: &str,
    claude: &str,
) -> Vec<std::path::PathBuf> {
    let compat = settings.get::<bool>("compat.claude").unwrap_or(true);
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
    let mut d = Vec::new();
    if trusted || kind == "prompts" {
        d.push(Paths::project_dir(cwd).join(kind));
    }
    if trusted && compat {
        d.push(cwd.join(".claude").join(claude));
    }
    d.push(paths.home().join(kind));
    if let Some(h) = &home {
        if compat {
            d.push(h.join(".claude").join(claude));
        }
        if kind == "skills" {
            d.push(h.join(".agents/skills"));
        }
    }
    d.extend(
        novacode_core::packages::enabled(paths.home(), cwd, trusted)
            .iter()
            .flat_map(|p| novacode_core::plugin::Parts::of(p).dirs(kind)),
    );
    d
}

/// What a package adds, in a line or two: its kind, parts and hook events.
fn package_summary(p: &novacode_core::plugin::Parts) -> String {
    use novacode_core::plugin::Kind;
    let count = |kind: &str| -> usize {
        match kind {
            "skills" => novacode_core::resources::skills(&p.dirs("skills")).0.len(),
            "prompts" => novacode_core::resources::templates(&p.dirs("prompts")).0.len(),
            _ => agents::defs(&p.dirs("agents")).0.len() - agents::defs(&[]).0.len(),
        }
    };
    let mut parts: Vec<String> = [("skills", "skills"), ("prompts", "commands"), ("agents", "agents")]
        .into_iter()
        .filter_map(|(k, label)| Some(count(k)).filter(|n| *n > 0).map(|n| format!("{n} {label}")))
        .collect();
    let servers: usize = p
        .mcp()
        .iter()
        .filter_map(|(_, r)| r.as_ref().ok())
        .map(|m| m.len())
        .sum();
    if servers > 0 {
        parts.push(format!("{servers} MCP servers"));
    }
    if p.kind == Kind::Native {
        if p.root.join("extensions").is_dir() {
            parts.push("extensions".into());
        }
        if p.root.join("themes").is_dir() {
            parts.push("themes".into());
        }
    }
    let kind = match (p.kind, p.version()) {
        (Kind::Claude, Some(v)) => format!("Claude Code plugin {v}"),
        (Kind::Claude, None) => "Claude Code plugin".into(),
        (Kind::Native, _) => "novacode package".into(),
    };
    let mut s = format!(
        "{kind}: {}",
        if parts.is_empty() {
            "nothing novacode loads".into()
        } else {
            parts.join(", ")
        }
    );
    let (hooks, skipped) = p.hooks();
    if !hooks.is_empty() {
        let mut ev: Vec<&str> = hooks.iter().map(|h| h.event.as_str()).collect();
        ev.dedup();
        s.push_str(&format!("\nhooks (run on every matching event): {}", ev.join(", ")));
    }
    if !skipped.is_empty() {
        s.push_str(&format!("\nnot supported: {}", skipped.join("; ")));
    }
    s
}

/// Ask for a plugin's declared options (`userConfig`) and save them. With
/// `keep`, options that already have a value are not asked again; without a
/// terminal the defaults are used.
fn ask_options(p: &novacode_core::plugin::Parts, keep: bool) -> anyhow::Result<()> {
    let specs = p.option_specs();
    if specs.is_empty() {
        return Ok(());
    }
    let mut values = p.options();
    if !std::io::stdin().is_terminal() {
        eprintln!(
            "`{}` has options; defaults are used (`novacode packages config {}` sets them)",
            p.name, p.name
        );
        return Ok(());
    }
    eprintln!("`{}` options (Enter keeps the value shown):", p.name);
    for (k, spec) in specs {
        if keep && values.contains_key(&k) && spec.get("default").is_none_or(|d| values.get(&k) != Some(d)) {
            continue;
        }
        let title = spec["title"].as_str().unwrap_or(&k);
        if let Some(d) = spec["description"].as_str() {
            eprintln!("  {d}");
        }
        let sensitive = spec["sensitive"] == true;
        let cur = match values.get(&k) {
            Some(_) if sensitive => " [set]".to_string(),
            Some(v) => format!(" [{}]", v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string())),
            None => String::new(),
        };
        let line = read_secret_or_line(&format!("  {title}{cur}: "), sensitive)?;
        if line.is_empty() {
            continue;
        }
        let v = match spec["type"].as_str() {
            Some("boolean") => serde_json::Value::Bool(matches!(
                line.to_lowercase().as_str(),
                "y" | "yes" | "true" | "1" | "on"
            )),
            Some("number") => line
                .parse::<f64>()
                .map(serde_json::Value::from)
                .unwrap_or(serde_json::Value::String(line)),
            _ => serde_json::Value::String(line),
        };
        values.insert(k, v);
    }
    p.save_options(&values).map_err(anyhow::Error::msg)?;
    eprintln!("saved; new sessions use them");
    Ok(())
}

fn read_secret_or_line(prompt: &str, secret: bool) -> anyhow::Result<String> {
    if secret {
        return read_secret(prompt);
    }
    eprint!("{prompt}");
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    Ok(line.trim().to_string())
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

/// `/restart`: exec the binary on disk again, resuming this session.
/// Returns only if exec failed.
/// After a rebuild or update, Linux names the running binary `<path> (deleted)`; the
/// new binary sits at `<path>`, which is what /restart is for.
fn live_path(exe: std::path::PathBuf) -> std::path::PathBuf {
    match exe.to_str().and_then(|s| s.strip_suffix(" (deleted)")) {
        Some(p) => p.into(),
        None => exe,
    }
}

fn restart(id: &str) -> anyhow::Error {
    use std::os::unix::process::CommandExt;
    let mut args = Vec::new();
    let mut skip = false;
    // `-w` takes an optional name: drop the next arg only if it isn't a flag.
    let mut skip_name = false;
    for a in std::env::args().skip(1) {
        if std::mem::take(&mut skip) || std::mem::take(&mut skip_name) && !a.starts_with('-') {
            continue;
        }
        match a.as_str() {
            "-c" | "--continue" => {}
            "-r" | "--resume" => skip = true,
            // The resumed session's folder decides where it runs (worktree included).
            "-w" | "--worktree" => skip_name = true,
            s if s.starts_with("--worktree=") || s.starts_with("-w") && s.len() > 2 => {}
            s if s.starts_with("--resume=") => {}
            _ => args.push(a),
        }
    }
    args.extend(["-r".into(), id.to_string()]);
    let exe = match std::env::current_exe() {
        Ok(e) => live_path(e),
        Err(e) => return e.into(),
    };
    anyhow::anyhow!("restart failed: {}", std::process::Command::new(exe).args(args).exec())
}

/// `--bg`: re-run this command detached, wait for its session id, print it.
/// Quitting inside a worktree: keep it (Enter) or remove it. Asked on the terminal after the TUI closed.
fn ask_remove(h: &novacode_tools::worktree::Here) -> bool {
    eprint!("Leaving {}.\nKeep the worktree? [Y/n] ", h.summary());
    let mut line = String::new();
    let _ = std::io::stdin().read_line(&mut line);
    matches!(line.trim(), "n" | "N" | "no")
}

/// `-w` without a name: `wt-MMDD-HHMM` (UTC).
fn chrono_name() -> String {
    let t = novacode_core::memory::utc_minute(session::now_ms());
    format!("wt-{}{}-{}{}", &t[5..7], &t[8..10], &t[11..13], &t[14..16])
}

const SHELL_FISH: &str = r#"function novacode --wraps novacode --description 'novacode; cd to the session folder on exit'
    set -l f (mktemp)
    NOVACODE_CWD_FILE=$f command novacode $argv
    set -l st $status
    set -l d (cat $f 2>/dev/null)
    rm -f $f
    if test -n "$d" -a -d "$d" -a "$d" != "$PWD"
        cd $d
    end
    return $st
end
"#;

const SHELL_SH: &str = r#"novacode() {
    local f st d
    f=$(mktemp) || return
    NOVACODE_CWD_FILE="$f" command novacode "$@"
    st=$?
    d=$(cat "$f" 2>/dev/null)
    rm -f "$f"
    if [ -n "$d" ] && [ -d "$d" ] && [ "$d" != "$PWD" ]; then cd "$d" || true; fi
    return $st
}
"#;

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

/// The window's terminal; given back to the shell when dropped (quit, restart, error).
struct Screen(novacode_tui::term::Term);

impl Drop for Screen {
    fn drop(&mut self) {
        novacode_tui::term::restore();
    }
}

/// The TUI, one session at a time; /resume and /new swap the session in place.
async fn interactive_loop(mut ctx: Ctx, mut store: Store, mut model: String) -> anyhow::Result<ExitCode> {
    let mut notices = Vec::new();
    let mut info: Vec<String> = Vec::new();
    let mut draft = String::new();
    // File rewinds to undo, newest last (L4.15).
    let undo: Arc<std::sync::Mutex<Vec<String>>> = Arc::default();
    let checkpoints = ctx
        .settings
        .get("checkpoints.enabled")
        .unwrap_or(true)
        .then(|| novacode_core::checkpoint::resolve(&ctx.cwd, ctx.paths.home()))
        .flatten();
    // Kept across reload, rewind and session swaps: the old frame stays up meanwhile.
    let mut screen: Option<Screen> = None;
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
        let tmux_pane = tmux_panel(&ctx.settings, &id, ctx.paths.home());
        let (etx, erx) = mpsc::unbounded_channel();
        // The window starts from the current extension UI, as attached windows do.
        for ev in host.hub.ui_log() {
            let _ = etx.send(ev);
        }
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
        // `novacode "prompt"`: sent once, as if typed into the first window.
        if let Some(p) = ctx.cli.initial.take() {
            let _ = ctx_tx.send(Cmd::Prompt(p));
        }
        let (mut commands, warn) = ctx.inputs().commands();
        commands.extend(host::MODE_COMMANDS.iter().map(|(c, d)| (c.to_string(), d.to_string())));
        commands.extend(host.extensions.iter().flat_map(|e| e.commands()));
        notices.extend(warn);
        notices.extend(novacode_core::packages::problems(
            ctx.paths.home(),
            &ctx.cwd,
            ctx.trusted,
        ));
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
            info: std::mem::take(&mut info),
            price: state.price,
            spent: state.spent,
            commands,
            draft: std::mem::take(&mut draft),
            attached: false,
            statusline: base.statusline,
            panel_only: false,
            // The tmux pane has the panel; the window keeps its full width.
            panel_auto: base.panel_auto && tmux_pane.is_none(),
            nerd_icons: base.nerd_icons,
            slow: base.slow,
            images: base.images.clone(),
        };
        let t = match &mut screen {
            Some(s) => s,
            None => {
                novacode_tui::term::install_panic_hook();
                screen.insert(Screen(novacode_tui::term::enter()?))
            }
        };
        let res = novacode_tui::run_in(&mut t.0, init, erx, ui_rx, ctx_tx).await;
        if matches!(res, Ok(Exit::Quit | Exit::Restart) | Err(_)) {
            screen = None;
        }
        handle.abort();
        handle.shutdown();
        let agent = task.await?;
        live.end(match &res {
            // Same session again: attached windows reconnect.
            Ok(
                Exit::Rewind { .. }
                | Exit::Reload
                | Exit::Restart
                | Exit::Branch(novacode_tui::app::BranchAction::Tree, _),
            ) => serve::RELOADING,
            Ok(Exit::New | Exit::Resume(_)) => "the session's window switched to another session",
            _ => "the session's window closed",
        })
        .await;
        if let Some(p) = &tmux_pane {
            let _ = std::process::Command::new("tmux").args(["kill-pane", "-t", p]).status();
        }
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
                            Ok(n) => info.push(format!("file rewind undone: {n} files restored")),
                            Err(e) => notices.push(format!("undo failed: {e}")),
                        },
                        None => info.push("nothing to undo".into()),
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
                            info.push(m);
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
            let old = agent.store.id().to_string();
            let (s, err) = branch(&ctx, agent.store, *action, at.as_deref(), &mut draft);
            store = s;
            use novacode_tui::app::BranchAction as A;
            if err.is_none() && matches!(action, A::Fork | A::Clone) {
                let verb = if at.is_none() { "cloned" } else { "forked" };
                info.push(format!(
                    "{verb} into a new session {}; the old one stays (novacode -r {})",
                    store.id(),
                    &old[..old.len().min(13)]
                ));
            }
            notices.extend(err);
            continue;
        }
        if res == Exit::Reload {
            notices.extend(ctx.reload());
            info.push("reloaded settings, keys, theme, extensions, MCP servers, skills and context files".into());
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
                // The agent keeps the process cwd on the session's folder (worktree moves included).
                let mut dir = std::env::current_dir().unwrap_or_else(|_| ctx.cwd.clone());
                let mut wt = novacode_tools::worktree::here(&dir)
                    .ok()
                    .and_then(|h| h.name().map(|n| (h, n)));
                if let Some((h, _)) = &wt
                    && ask_remove(h)
                {
                    eprintln!("{}", novacode_tools::worktree::remove(h).trim());
                    dir = h.main().to_path_buf();
                    wt = None;
                }
                if prev.as_ref().is_some_and(|p| p.exists()) {
                    let w = wt.map(|(_, n)| format!("-w {n} ")).unwrap_or_default();
                    eprintln!("resume with: novacode {w}-r {}", &id[..id.len().min(13)]);
                }
                // `novacode shell-init`'s function cd's here after we exit.
                if let Some(f) = std::env::var_os("NOVACODE_CWD_FILE") {
                    let _ = std::fs::write(f, dir.as_os_str().as_encoded_bytes());
                }
                return Ok(ExitCode::SUCCESS);
            }
            Exit::New => Ok(Store::create(&ctx.paths, &ctx.cwd, ctx.cli.no_session)),
            Exit::Branch(..) | Exit::Rewind { .. } | Exit::Reload | Exit::Restart => unreachable!("handled above"),
            Exit::Resume(path) => match Store::open(Path::new(&path)) {
                Ok(s) => {
                    let dir = s.last_cwd();
                    if dir.is_dir() && Some(&dir) != std::env::current_dir().ok().as_ref() {
                        // Another folder: start over there so its trust, settings and context files load.
                        std::env::set_current_dir(&dir)?;
                        let id = s.header.id.clone();
                        drop(s); // release the session lock before exec
                        drop(screen);
                        return Err(restart(&id));
                    }
                    Ok(s)
                }
                e => e,
            },
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

/// `/tree`, `/rewind`, `/fork`, `/clone`: move the leaf or fork a new
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

/// Write a session's branch as JSONL or HTML, secrets masked.
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
/// A trusted project declares packages that aren't installed: ask once per
/// project, never install silently. The answer is kept in `~/.novacode/project-packages.json`.
async fn project_packages(paths: &Paths, cwd: &Path, interactive: bool) -> anyhow::Result<()> {
    let missing = novacode_core::packages::missing(cwd);
    if missing.is_empty() {
        return Ok(());
    }
    let file = paths.home().join("project-packages.json");
    let mut asked: std::collections::BTreeMap<String, bool> = std::fs::read_to_string(&file)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default();
    let key = cwd.display().to_string();
    if !interactive || asked.contains_key(&key) {
        eprintln!(
            "note: this project wants packages that aren't installed ({}); run `novacode packages install`",
            missing.join(", ")
        );
        return Ok(());
    }
    let pick = novacode_tui::prompt::select(
        "Install this project's packages?",
        &[
            format!("{} declares packages in .novacode/packages.json:", cwd.display()),
            missing.join(", "),
            "They can add tools, hooks and MCP servers that run code.".into(),
        ],
        vec![
            PickItem {
                label: "Install them".into(),
                detail: String::new(),
                value: "yes".into(),
            },
            PickItem {
                label: "Not now (don't ask again here)".into(),
                detail: String::new(),
                value: "no".into(),
            },
        ],
    )?;
    let yes = pick.as_deref() == Some("yes");
    asked.insert(key, yes);
    novacode_base::storage::write_atomic(
        &file,
        serde_json::to_string_pretty(&asked)?.as_bytes(),
        novacode_base::storage::Durability::Durable,
    )?;
    if yes {
        for (n, r) in novacode_core::packages::install_project(cwd).await {
            match r {
                Ok(_) => eprintln!("installed `{n}`"),
                Err(e) => eprintln!("`{n}` failed: {e}"),
            }
        }
    }
    Ok(())
}

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
/// whichever comes first. Returns (code, state).
async fn loopback_or_paste(port: u16) -> anyhow::Result<(String, Option<String>)> {
    catch_or_paste(tokio::net::TcpListener::bind(("127.0.0.1", port)).await.ok()).await
}

async fn catch_or_paste(listener: Option<tokio::net::TcpListener>) -> anyhow::Result<(String, Option<String>)> {
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
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
    fn restart_uses_the_rebuilt_binary() {
        let p = |s: &str| super::live_path(s.into());
        assert_eq!(p("/x/novacode (deleted)"), std::path::PathBuf::from("/x/novacode"));
        assert_eq!(p("/x/novacode"), std::path::PathBuf::from("/x/novacode"));
    }

    #[test]
    fn percent_decoding() {
        assert_eq!(super::percent_decode("4%2F0Ab-x%3D"), "4/0Ab-x=");
        assert_eq!(super::percent_decode("plain%zz"), "plain%zz");
    }
}
