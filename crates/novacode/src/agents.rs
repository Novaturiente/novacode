//! Subagents (L10.1): the `agent` tool, agent definitions and file claims.
//!
//! Each child is `novacode --rpc` with piped stdio, speaking the L8.7 protocol.
//! The parent reads its events, relays its questions and approvals to the
//! parent's own windows, and saves its final reply as a report. A child dies
//! with its parent: the parent holding the pipe is what keeps it alive.

use crate::hub::HubSlot;
use crate::wire::ServerMsg;
use futures_util::future::BoxFuture;
use novacode_core::hooks::{Gate, Handler};
use novacode_core::policy::{Approval, ApprovalRequest, Approver};
use novacode_core::resources;
use novacode_tools::ask::Asker;
use novacode_tools::{Tool, ToolCtx, ToolOutput};
use novacode_types::ui::{Node, Time, UiOp};
use novacode_types::{AgentEvent, Message, StopReason, StreamEvent};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::{BTreeMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{Notify, mpsc};

/// Children may spawn children; theirs may not (L10.1 depth 2).
pub const MAX_DEPTH: u32 = 2;
/// The running binary, for children. After an update deletes it, Linux still runs it
/// as `/proc/self/exe`, and a child of the same version speaks the same `--rpc`.
fn own_exe() -> PathBuf {
    let exe = std::env::current_exe().unwrap_or_else(|_| "novacode".into());
    let proc = Path::new("/proc/self/exe");
    if exe.to_str().is_some_and(|s| s.ends_with(" (deleted)")) && proc.exists() {
        return proc.into();
    }
    exe
}

/// `(provider, wanted)` → `provider/model`: `small` is the provider's small model,
/// a family (`sonnet`) or a bare model id is looked up on that provider. None: not there.
pub type ModelLookup = Arc<dyn Fn(&str, &str) -> Option<String> + Send + Sync>;

/// A child's model: the spawn's or its type's (`provider/model` as is; anything
/// else on the parent's current provider), else the parent's own.
fn child_model(
    requested: Option<&str>,
    parent: String,
    lookup: &(dyn Fn(&str, &str) -> Option<String> + Send + Sync),
) -> String {
    match requested {
        Some(m) if m.contains('/') => m.to_string(),
        Some(m) => lookup(parent.split_once('/').map_or("", |(p, _)| p), m).unwrap_or(parent),
        None => parent,
    }
}

/// Reports longer than this reach the model cut, with an `agent read` pointer.
/// Reports are written terse (see the task preamble), so most arrive whole; only
/// a runaway one is cut, saving the parent an `agent read` call per child.
const REPORT_CUT: usize = 6000;

/// This process's place in an agent tree, from the environment its parent set.
#[derive(Debug, Clone, Default)]
pub struct Place {
    pub depth: u32,
    /// Session ids from the root down to the parent.
    pub chain: Vec<String>,
}

impl Place {
    pub fn from_env() -> Self {
        Place {
            depth: std::env::var("NOVACODE_AGENT_DEPTH")
                .ok()
                .and_then(|d| d.parse().ok())
                .unwrap_or(0),
            chain: std::env::var("NOVACODE_AGENT_CHAIN")
                .unwrap_or_default()
                .split(',')
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect(),
        }
    }

    /// The root session of this tree (this one, at depth 0).
    pub fn root<'a>(&'a self, me: &'a str) -> &'a str {
        self.chain.first().map_or(me, String::as_str)
    }
}

// ---------------------------------------------------------------------------
// Agent definitions

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Def {
    pub name: String,
    pub description: String,
    /// Its default tools; None: all of the parent's. The child is built with only these.
    pub tools: Option<Vec<String>>,
    /// MCP servers it gets by default (`agents.<name>.mcp`, Claude `mcpServers`); none otherwise.
    pub mcp: Vec<String>,
    /// `provider/model`, `small` for the provider's small model, or a model or
    /// family (`sonnet`) on the parent's provider.
    pub model: Option<String>,
    pub thinking: Option<String>,
    pub prompt: String,
    /// The prompt replaces the preamble instead of being added after it.
    pub replace: bool,
    /// Claude `disallowedTools`: never given, whatever the spawn asks (`mcp__server…` drops the server).
    pub disallowed: Vec<String>,
    /// Claude `permissionMode: plan`: starts read-only.
    pub read_only: bool,
    /// Claude `maxTurns`: model turns before it is stopped with a partial report.
    pub max_turns: Option<u32>,
    /// Claude `skills`: their text starts the task.
    pub skills: Vec<String>,
    /// Claude `isolation: worktree`.
    pub worktree: bool,
    /// Claude `omitClaudeMd`: no AGENTS.md/CLAUDE.md files.
    pub no_context: bool,
}

/// Claude Code agent fields novacode doesn't act on (L7.16); `hooks` refuses the file.
const AGENT_UNSUPPORTED: &[&str] = &[
    "memory",
    "background",
    "initialPrompt",
    "autoCompactWindow",
    "experimental",
];

fn builtins() -> Vec<Def> {
    let def = |name: &str, description: &str, tools: Option<&[&str]>, model: Option<&str>, prompt: &str| Def {
        name: name.into(),
        description: description.into(),
        tools: tools.map(|t| t.iter().map(|s| s.to_string()).collect()),
        mcp: Vec::new(),
        model: model.map(str::to_string),
        prompt: prompt.into(),
        ..Default::default()
    };
    vec![
        def(
            "scout",
            "Fast read-only recon: where/how is X done, map a folder, summarise long files or logs.",
            Some(&["read", "grep", "find", "ls", "todo"]),
            Some("small"),
            "You are a scout. Explore and answer; never edit. Report compactly: findings with file:line references, then open questions.",
        ),
        def(
            "worker",
            "A bounded change: edit the named files, run the check, report what changed.",
            None,
            None,
            "You are a worker. Make exactly the change asked, nothing more. Verify it with the project's build or tests. Report: files changed, the check you ran and its result, anything left open.",
        ),
        def(
            "reviewer",
            "Review a change for bugs, risky code and missing tests; changes nothing.",
            Some(&["read", "grep", "find", "ls", "bash", "todo"]),
            None,
            "You are a reviewer. Read the change and the code it touches. Report findings ranked by severity, each with file:line and a one-line fix. Run only read-only commands (git diff, git log, tests). Never edit.",
        ),
        def(
            "auditor",
            "Checks that a goal is really done (used by the goal tool); changes nothing.",
            Some(&["read", "grep", "find", "ls", "bash"]),
            None,
            "You are a completion auditor. You get a goal, its completion criteria and the agent's evidence. Don't trust the evidence: check each criterion yourself in the workspace (read files, run the tests or commands that prove it; read-only commands only, never edit). Your first line must be exactly `VERDICT: approved` or `VERDICT: rejected`. Then one line per criterion: met or not met, with what you checked. If rejected, end with the concrete work still needed.",
        ),
    ]
}

/// Agent files from `dirs` (first wins a name, L7.12), then the built-ins.
/// Returns the shadowed files too (for `doctor`).
pub fn defs(dirs: &[PathBuf]) -> (Vec<Def>, Vec<String>) {
    let mut all: BTreeMap<String, Def> = BTreeMap::new();
    let mut seen = novacode_core::resources::Seen::default();
    for dir in dirs {
        // A plugin's manifest may name agent files, not folders (L7.14).
        let entries = if dir.is_file() {
            vec![dir.clone()]
        } else {
            std::fs::read_dir(dir)
                .into_iter()
                .flatten()
                .flatten()
                .map(|e| e.path())
                .collect()
        };
        let mut files: Vec<PathBuf> = entries
            .into_iter()
            .filter(|p| p.extension().is_some_and(|e| e == "md"))
            .collect();
        files.sort();
        for f in files {
            let Ok(text) = std::fs::read_to_string(&f) else {
                continue;
            };
            let text = novacode_core::plugin::fill_for(&f, text);
            let (fm, body, valid) = resources::parse_frontmatter(&text);
            let name = resources::field(&fm, "name")
                .or_else(|| f.file_stem().map(|s| s.to_string_lossy().to_string()))
                .unwrap_or_default();
            if name.is_empty() {
                continue;
            }
            let Some(name) = seen.admit("agent", &name, &f) else {
                continue;
            };
            let mut note = |s: String| seen.shadowed.push(format!("agent `{name}`: {s}"));
            if !valid {
                note(format!(
                    "frontmatter isn't valid YAML; read as plain `key: value` lines ({})",
                    f.display()
                ));
            }
            // Claude Code ignores these three in plugin agents; so do we.
            let plugin = novacode_core::plugin::owner_name(&f).is_some();
            if fm.contains_key("hooks") && !plugin {
                note("not loaded: its `hooks` don't run in novacode, and it may rely on them".into());
                all.remove(&name);
                continue;
            }
            let mut d = Def::default();
            // One MCP tool can't be granted alone: its server comes whole.
            let wide: Vec<String> = resources::list(fm.get("tools"))
                .into_iter()
                .filter_map(|t| {
                    let (server, tool) = t.strip_prefix("mcp__")?.split_once("__")?;
                    (!tool.is_empty()).then(|| format!("mcp__{server}__{tool} gives all of {server}'s tools"))
                })
                .collect();
            // `mcp__server__tool` names grant the server: MCP tools come per server.
            let split = |names: Vec<String>, mcp: &mut Vec<String>| -> Vec<String> {
                names
                    .into_iter()
                    .filter_map(|t| match t.strip_prefix("mcp__") {
                        Some(rest) => {
                            mcp.push(rest.split("__").next().unwrap_or(rest).to_string());
                            None
                        }
                        None => Some(crate::import::nova_tool(&t)),
                    })
                    .collect()
            };
            d.tools = fm.get("tools").map(|v| split(resources::list(Some(v)), &mut d.mcp));
            let mut dropped = Vec::new();
            d.disallowed = split(resources::list(fm.get("disallowedTools")), &mut dropped);
            d.disallowed.extend(dropped.into_iter().map(|s| format!("mcp__{s}")));
            if !plugin {
                for s in resources::list(fm.get("mcpServers")) {
                    d.mcp.push(s);
                }
                if fm
                    .get("mcpServers")
                    .and_then(Value::as_array)
                    .is_some_and(|a| a.iter().any(Value::is_object))
                {
                    note("inline `mcpServers` definitions are not supported; only server names".into());
                }
                match resources::field(&fm, "permissionMode").as_deref() {
                    Some("plan") => d.read_only = true,
                    None | Some("default" | "manual") => {}
                    Some(m) => note(format!(
                        "`permissionMode: {m}` ignored: a subagent never gets more than its parent"
                    )),
                }
            }
            d.mcp.sort();
            d.mcp.dedup();
            if !wide.is_empty() {
                note(format!("{} (MCP tools come per server)", wide.join("; ")));
            }
            d.model = resources::field(&fm, "model").filter(|m| m != "inherit");
            d.thinking = resources::field(&fm, "thinking").or_else(|| resources::field(&fm, "effort"));
            d.max_turns = fm.get("maxTurns").and_then(Value::as_u64).map(|n| n as u32);
            d.skills = resources::list(fm.get("skills"));
            d.worktree = resources::field(&fm, "isolation").as_deref() == Some("worktree");
            d.no_context = resources::flag(fm.get("omitClaudeMd"));
            let ignored: Vec<&str> = AGENT_UNSUPPORTED
                .iter()
                .copied()
                .filter(|k| fm.contains_key(*k))
                .collect();
            if !ignored.is_empty() {
                note(format!("not supported, ignored: {}", ignored.join(", ")));
            }
            all.insert(
                name.clone(),
                Def {
                    name,
                    description: resources::field(&fm, "description").unwrap_or_default(),
                    prompt: body.trim().to_string(),
                    replace: resources::field(&fm, "mode")
                        .or(resources::field(&fm, "systemPromptMode"))
                        .is_some_and(|m| m == "replace"),
                    ..d
                },
            );
        }
    }
    for d in builtins() {
        all.entry(d.name.clone()).or_insert(d);
    }
    (all.into_values().collect(), seen.shadowed)
}

/// The user's per-type settings (`agents.<name>`).
#[derive(Debug, Default, Deserialize)]
pub struct AgentCfg {
    pub tools: Option<Vec<String>>,
    pub mcp: Option<Vec<String>>,
}

/// Apply `agents.<name>` settings: they replace a definition's tools and MCP servers.
pub fn configure(defs: &mut [Def], cfg: impl Fn(&str) -> Option<AgentCfg>) {
    for d in defs {
        let Some(c) = cfg(&d.name) else { continue };
        if let Some(t) = c.tools {
            d.tools = Some(t.iter().map(|s| crate::import::nova_tool(s.trim())).collect());
        }
        if let Some(m) = c.mcp {
            d.mcp = m;
        }
    }
}

/// What a child gets (L10.1): the parent's pick when the user allows it, else its
/// type's defaults; never a tool the parent lacks (`own`; None: not known). MCP
/// servers only when granted. Returns (tools, servers, notes for the parent).
pub fn grant(
    def: &Def,
    pick_tools: Option<Vec<String>>,
    pick_mcp: Option<Vec<String>>,
    parent_picks: bool,
    own: Option<&[String]>,
    servers: &[String],
) -> (Option<Vec<String>>, Vec<String>, Vec<String>) {
    let mut notes = Vec::new();
    let (pick_tools, pick_mcp) = if parent_picks {
        (pick_tools, pick_mcp)
    } else {
        if pick_tools.is_some() || pick_mcp.is_some() {
            notes.push("`tools`/`mcp` ignored: the user sets subagent tools (agents.parentPicksTools is off)".into());
        }
        (None, None)
    };
    let picked = pick_tools.is_some();
    let pick_tools = pick_tools.map(|t| t.iter().map(|s| crate::import::nova_tool(s.trim())).collect());
    let mut tools = pick_tools
        .or_else(|| def.tools.clone())
        .or_else(|| own.map(<[String]>::to_vec));
    if let (Some(t), Some(own)) = (tools.as_mut(), own) {
        let (keep, lack): (Vec<String>, Vec<String>) = std::mem::take(t).into_iter().partition(|n| own.contains(n));
        if picked && !lack.is_empty() {
            let hint = if lack.iter().any(|n| n.contains("__")) {
                " (MCP tools come with `mcp: [server]`)"
            } else {
                ""
            };
            notes.push(format!("not given, you don't have them: {}{hint}", lack.join(", ")));
        }
        *t = keep;
    }
    // Claude `disallowedTools`: never given, whoever picks (no `bash`: no `process` either).
    if let Some(t) = tools.as_mut() {
        let no_shell = def.disallowed.iter().any(|d| d == "bash");
        t.retain(|n| !def.disallowed.contains(n) && !(no_shell && n == "process"));
    }
    let granted = pick_mcp.is_some();
    let (mut mcp, unknown): (Vec<String>, Vec<String>) = pick_mcp
        .unwrap_or_else(|| def.mcp.clone())
        .into_iter()
        .partition(|s| servers.contains(s));
    if granted && !unknown.is_empty() {
        notes.push(format!(
            "no MCP server {}; configured: {}",
            unknown.join(", "),
            if servers.is_empty() {
                "none".into()
            } else {
                servers.join(", ")
            }
        ));
    }
    mcp.retain(|s| !def.disallowed.contains(&format!("mcp__{s}")));
    // MCP tools run through codemode unless configured as listed.
    if !mcp.is_empty()
        && let Some(t) = tools.as_mut()
        && !t.iter().any(|n| n == "codemode")
        && own.is_none_or(|o| o.iter().any(|n| n == "codemode"))
    {
        t.push("codemode".into());
        notes.push("added codemode: MCP tools run through it".into());
    }
    (tools, mcp, notes)
}

// ---------------------------------------------------------------------------
// File claims (L10.1): one file per agent tree under `run/claims/`.

#[derive(Debug, Clone, serde::Serialize, Deserialize)]
struct Claim {
    path: String,
    holder: String,
    title: String,
    pid: u32,
}

pub struct Claims {
    file: PathBuf,
    me: String,
    title: String,
    place: Place,
    cwd: PathBuf,
}

impl Claims {
    pub fn new(run_dir: &Path, me: &str, place: Place, cwd: &Path) -> Self {
        Claims {
            file: run_dir.join("claims").join(format!("{}.json", place.root(me))),
            me: me.into(),
            title: std::env::var("NOVACODE_AGENT_TITLE").unwrap_or_else(|_| "main".into()),
            place,
            cwd: cwd.into(),
        }
    }

    fn paths(&self, tool: &str, args: &Value) -> Vec<PathBuf> {
        let rel: Vec<String> = match tool {
            "edit" | "write" => args["path"].as_str().map(|p| vec![p.to_string()]).unwrap_or_default(),
            "apply_patch" => args["patch"]
                .as_str()
                .or(args["input"].as_str())
                .unwrap_or_default()
                .lines()
                .filter_map(|l| {
                    ["*** Update File: ", "*** Add File: ", "*** Delete File: "]
                        .iter()
                        .find_map(|p| l.strip_prefix(p))
                })
                .map(|p| p.trim().to_string())
                .collect(),
            _ => Vec::new(),
        };
        rel.into_iter()
            .map(|p| {
                let p = at(&self.cwd, &p);
                // A new file has no real path yet: resolve its folder.
                std::fs::canonicalize(&p).unwrap_or_else(|_| {
                    p.parent()
                        .and_then(|d| std::fs::canonicalize(d).ok())
                        .map(|d| d.join(p.file_name().unwrap_or_default()))
                        .unwrap_or(p)
                })
            })
            .collect()
    }

    /// Refuse a path another live agent holds; otherwise claim it (children only).
    fn check(&self, paths: &[PathBuf]) -> Result<(), String> {
        if paths.is_empty() {
            return Ok(());
        }
        let _ = std::fs::create_dir_all(self.file.parent().unwrap_or(Path::new(".")));
        let _lock = novacode_base::storage::lock(&self.file).map_err(|e| e.to_string())?;
        let mut all: Vec<Claim> = novacode_base::storage::read_json(&self.file)
            .ok()
            .flatten()
            .unwrap_or_default();
        all.retain(|c| alive(c.pid));
        for p in paths {
            let p = p.display().to_string();
            if let Some(c) = all.iter().find(|c| c.path == p && c.holder != self.me) {
                // A parent's claim passes down to its children.
                if !self.place.chain.contains(&c.holder) {
                    return Err(format!(
                        "{p} is being edited by subagent \"{}\" ({}). Leave it to them, or wait until it finishes.",
                        c.title,
                        &c.holder[..c.holder.len().min(13)]
                    ));
                }
            } else if self.place.depth > 0 && !all.iter().any(|c| c.path == p) {
                all.push(Claim {
                    path: p,
                    holder: self.me.clone(),
                    title: self.title.clone(),
                    pid: std::process::id(),
                });
            }
        }
        if all.is_empty() {
            // Nobody holds anything: leave no file behind (claims are runtime state).
            let _ = std::fs::remove_file(&self.file);
            let _ = std::fs::remove_file(novacode_base::storage::backup_path(&self.file));
            return Ok(());
        }
        novacode_base::storage::write_json(&self.file, &all, novacode_base::storage::Durability::Fast)
            .map_err(|e| e.to_string())
    }
}

fn alive(pid: u32) -> bool {
    novacode_tools::process::pid_alive(pid)
}

impl Handler for Claims {
    fn before_tool<'a>(&'a self, tool: &'a str, args: &'a Value) -> BoxFuture<'a, Gate> {
        Box::pin(async move {
            match self.check(&self.paths(tool, args)) {
                Ok(()) => Gate::Allow,
                Err(e) => Gate::Block(e),
            }
        })
    }
}

// ---------------------------------------------------------------------------
// Children

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Status {
    Queued,
    Starting,
    Running,
    Waiting,
    Done,
    Failed,
    Stopped,
}

impl Status {
    fn word(self) -> &'static str {
        match self {
            Status::Queued => "queued",
            Status::Starting => "starting",
            Status::Running => "running",
            Status::Waiting => "waiting",
            Status::Done => "done",
            Status::Failed => "failed",
            Status::Stopped => "stopped",
        }
    }
    fn finished(self) -> bool {
        matches!(self, Status::Done | Status::Failed | Status::Stopped)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
enum Notify_ {
    /// Start a turn when the report arrives.
    #[default]
    Wake,
    /// Ride along with the next turn.
    Next,
    /// Panel only.
    Quiet,
}

struct Child {
    /// Short handle the model uses: a1, a2…
    n: String,
    title: String,
    agent: String,
    model: String,
    task: String,
    cwd: PathBuf,
    /// Its tools and MCP servers (kept for resumes); None: all of its process's.
    tools: Option<Vec<String>>,
    mcp: Vec<String>,
    notify: Notify_,
    after: Vec<String>,
    status: Status,
    session: Option<String>,
    started: Option<Instant>,
    started_ms: u64,
    ended_ms: Option<u64>,
    last_activity: Instant,
    /// Calls of its `agent` tool in action `wait` still running: exempt from the stall check.
    waits: Vec<String>,
    /// Relayed questions and approvals still open; aborted when it ends (the card closes).
    relays: Vec<tokio::task::AbortHandle>,
    /// Lines to its stdin; dropping it ends the child.
    tx: Option<mpsc::UnboundedSender<String>>,
    pid: Option<u32>,
    last_text: String,
    report: Option<String>,
    log: VecDeque<String>,
    tokens: u64,
    tool_calls: u32,
    /// The report was handed over (by `wait` or as a reminder).
    delivered: bool,
    /// A message to resume it with once a slot is free (`send` to a finished child).
    resume: Option<String>,
    /// From its definition: starts read-only, without context files, stops after this many turns.
    read_only: bool,
    no_context: bool,
    max_turns: Option<u32>,
    /// Model turns in the current run.
    turns: u32,
}

pub struct Supervisor {
    children: Mutex<Vec<Child>>,
    /// Children ever spawned: the next id (a1, a2…) without holding `children`.
    next: std::sync::atomic::AtomicUsize,
    changed: Notify,
    place: Place,
    me: String,
    home: PathBuf,
    exe: PathBuf,
    waiting: Mutex<usize>,
    /// What `/reload` may change; running children stay (`reconfigure`).
    cfg: std::sync::RwLock<Arc<Cfg>>,
}

#[derive(Clone)]
struct Cfg {
    /// The parent session: its windows answer the children's questions.
    slot: HubSlot,
    cwd: PathBuf,
    trusted: bool,
    stall: Duration,
    timeout: Duration,
    defs: Vec<Def>,
    /// The model may pick a child's tools and MCP servers (`agents.parentPicksTools`).
    parent_picks: bool,
    /// MCP servers this session may hand on.
    servers: Vec<String>,
    /// This session's own tool names (MCP ones aside): a child never gets more.
    own: Option<Vec<String>>,
    models: ModelLookup,
    skill_dirs: Vec<PathBuf>,
    /// Children running at once; more wait as `queued` (`agents.maxConcurrent`; 1 below the top).
    max_running: usize,
    /// Nerd Font glyphs in the panel rows (`ui.icons`).
    nerd: bool,
    /// Print mode: nobody reads notices, so every report wakes the parent.
    unattended: bool,
    /// Extra environment for every child (`agents.env`), e.g. to switch a plugin off.
    env: std::collections::BTreeMap<String, String>,
    /// The session's policy: children start in its effective mode, capped by its preset's tools.
    policy: Arc<novacode_core::policy::Policy>,
}

impl Cfg {
    fn of(s: Setup) -> Self {
        Cfg {
            slot: s.slot,
            defs: s.defs,
            parent_picks: s.parent_picks,
            servers: s.servers,
            own: None,
            cwd: s.cwd,
            trusted: s.trusted,
            stall: Duration::from_secs(s.stall_min * 60),
            timeout: Duration::from_secs(s.timeout_min * 60),
            models: s.models,
            skill_dirs: s.skill_dirs,
            max_running: s.max_running.max(1),
            nerd: s.nerd,
            unattended: s.unattended,
            env: s.env,
            policy: s.policy,
        }
    }
}

pub struct Setup {
    pub slot: HubSlot,
    pub me: String,
    pub home: PathBuf,
    pub cwd: PathBuf,
    pub trusted: bool,
    /// Agent definitions, settings applied (`defs` + `configure`).
    pub defs: Vec<Def>,
    pub parent_picks: bool,
    pub servers: Vec<String>,
    pub stall_min: u64,
    pub timeout_min: u64,
    /// A provider's small model as `provider/model`, asked at spawn time (the parent may switch provider).
    pub models: ModelLookup,
    /// Skill folders, for agents that preload skills (Claude `skills`).
    pub skill_dirs: Vec<PathBuf>,
    pub max_running: usize,
    /// The child binary (tests use a fake); default: this one.
    pub exe: Option<PathBuf>,
    pub nerd: bool,
    pub unattended: bool,
    pub env: std::collections::BTreeMap<String, String>,
    pub policy: Arc<novacode_core::policy::Policy>,
}

impl Supervisor {
    pub fn new(s: Setup) -> Arc<Self> {
        let sup = Arc::new(Supervisor {
            children: Mutex::default(),
            next: Default::default(),
            changed: Notify::new(),
            place: Place::from_env(),
            me: s.me.clone(),
            home: s.home.clone(),
            exe: s.exe.clone().unwrap_or_else(own_exe),
            waiting: Mutex::new(0),
            cfg: std::sync::RwLock::new(Arc::new(Cfg::of(s))),
        });
        // Stall and timeout watchdog (L10.1: 10 min without activity, 45 min in all).
        let weak = Arc::downgrade(&sup);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(15)).await;
                let Some(s) = weak.upgrade() else { break };
                for (n, why) in s.overdue() {
                    s.stop(&n, why);
                }
            }
        });
        sup
    }

    /// Children the watchdog stops, and why.
    fn overdue(&self) -> Vec<(String, &'static str)> {
        let cfg = self.c();
        let mut stop = Vec::new();
        for c in self.children.lock().unwrap().iter() {
            if matches!(c.status, Status::Running | Status::Starting) {
                // Waiting on its own children is not a stall; the overall timeout still applies.
                if c.waits.is_empty() && c.last_activity.elapsed() > cfg.stall {
                    stop.push((c.n.clone(), "stalled: no activity"));
                } else if c.started.is_some_and(|t| t.elapsed() > cfg.timeout) {
                    stop.push((c.n.clone(), "timed out"));
                }
            }
        }
        stop
    }

    /// The parent session ends: stop every child and give them up to 3 s in all to exit.
    pub async fn stop_all(self: &Arc<Self>) {
        let live: Vec<(String, Option<u32>)> = self
            .children
            .lock()
            .unwrap()
            .iter()
            .filter(|c| !c.status.finished())
            .map(|c| (c.n.clone(), c.pid))
            .collect();
        for (n, _) in &live {
            self.stop(n, "the parent session ended");
        }
        let pids: Vec<u32> = live.into_iter().filter_map(|(_, p)| p).collect();
        let deadline = Instant::now() + Duration::from_secs(3);
        while pids.iter().any(|p| alive(*p)) && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        // `stop`'s delayed SIGTERM may never run once the runtime ends; send it now.
        for p in pids.into_iter().filter(|p| alive(*p)) {
            let _ = std::process::Command::new("kill").arg(p.to_string()).status();
        }
    }

    /// This session's tool names, once its tool list is built (children get no more).
    pub fn set_own_tools(&self, names: Vec<String>) {
        let mut g = self.cfg.write().unwrap();
        let mut c = Cfg::clone(&g);
        c.own = Some(names);
        *g = Arc::new(c);
    }

    /// `/reload`: new settings, agent types and window; running children carry on.
    pub fn reconfigure(&self, s: Setup) {
        *self.cfg.write().unwrap() = Arc::new(Cfg::of(s));
    }

    fn c(&self) -> Arc<Cfg> {
        self.cfg.read().unwrap().clone()
    }

    fn handle(&self) -> Option<novacode_core::agent::SessionHandle> {
        self.c()
            .slot
            .0
            .get()
            .and_then(|w| w.upgrade())
            .map(|h| h.handle.clone())
    }

    fn parent_model(&self) -> Option<String> {
        self.c().slot.0.get().and_then(|w| w.upgrade()).map(|h| h.state().model)
    }

    /// Show a child in the Agent tab (L9.16 Subagents).
    fn publish(&self, c: &Child) {
        let Some(h) = self.handle() else { return };
        // Like the tmux swarm pane: type + id, then the title, then tokens and tool
        // calls; while it works, what it's doing.
        let (bot, db, wrench) = if self.c().nerd {
            ("\u{f06a9} ", "\u{f01bc} ", "\u{f0ad} ")
        } else {
            ("", "", "tools ")
        };
        let mut detail = vec![c.title.clone()];
        if c.tokens > 0 || c.tool_calls > 0 {
            detail.push(format!("{db}{}k  {wrench}{}", c.tokens.div_ceil(1000), c.tool_calls));
        }
        if !c.status.finished()
            && let Some(l) = c.log.back()
        {
            detail.push(format!("› {l}"));
        }
        let node = Node {
            id: c.n.clone(),
            parent: None,
            label: format!("{bot}{}", c.agent),
            tag: Some(c.n.clone()),
            status: Some(c.status.word().into()),
            icon: None,
            dim: c.status.finished(),
            detail,
            time: c.started.map(|_| match c.ended_ms {
                Some(e) => Time {
                    ms: Some(e.saturating_sub(c.started_ms)),
                    since: None,
                },
                None => Time {
                    ms: None,
                    since: Some(c.started_ms),
                },
            }),
            order: Some(c.n[1..].parse().unwrap_or(0.0)),
            actions: BTreeMap::new(),
        };
        h.emit(AgentEvent::Ui {
            ext: "agents".into(),
            op: UiOp::Publish {
                tab: "agent".into(),
                source: "subagents".into(),
                node: Box::new(node),
            },
        });
    }

    fn with<R>(&self, n: &str, f: impl FnOnce(&mut Child) -> R) -> Option<R> {
        let mut g = self.children.lock().unwrap();
        // A name wins over a session-id prefix: a session starting `a3…` must not catch `a3`.
        let i = g.iter().position(|c| c.n == n).or_else(|| {
            g.iter()
                .position(|c| c.session.as_deref().is_some_and(|s| s.starts_with(n)))
        })?;
        let c = &mut g[i];
        let r = f(c);
        self.publish(c);
        drop(g);
        self.changed.notify_waiters();
        Some(r)
    }

    /// Run the `auditor` agent on `task` and return its whole report (the goal tool).
    pub async fn audit(self: &Arc<Self>, task: String, title: String, model: Option<String>) -> Result<String, String> {
        let id = self
            .spawn(SpawnArgs {
                task,
                title: Some(title),
                agent: Some("auditor".into()),
                model,
                cwd: None,
                after: Vec::new(),
                notify: Notify_::Quiet,
                worktree: false,
                tools: None,
                mcp: None,
            })?
            .0;
        self.wait(vec![id.clone()], false, self.c().timeout).await;
        let g = self.children.lock().unwrap();
        let c = g.iter().find(|c| c.n == id).ok_or("the auditor vanished")?;
        match (c.status, &c.report) {
            (Status::Done, Some(r)) => Ok(r.clone()),
            _ => Err(format!("the auditor didn't finish ({})", c.status.word())),
        }
    }

    /// A skill with `context: fork` (L5.25): its text as the task of an `agent` subagent.
    pub fn spawn_task(self: &Arc<Self>, task: String, agent: String, title: String) -> Result<String, String> {
        let (id, notes) = self.spawn(SpawnArgs {
            task,
            title: Some(title),
            agent: Some(agent),
            model: None,
            cwd: None,
            after: Vec::new(),
            notify: Notify_::default(),
            worktree: false,
            tools: None,
            mcp: None,
        })?;
        let notes = if notes.is_empty() {
            String::new()
        } else {
            format!(" ({})", notes.join("; "))
        };
        Ok(format!("started {id}; its report arrives when it's done{notes}"))
    }

    /// Start (or queue) a child; returns its id and notes about its tools.
    fn spawn(self: &Arc<Self>, a: SpawnArgs) -> Result<(String, Vec<String>), String> {
        if self.place.depth >= MAX_DEPTH {
            return Err(format!("subagents can't spawn below depth {MAX_DEPTH}"));
        }
        let agent = a.agent.unwrap_or_else(|| "worker".into());
        let cfg = self.c();
        let def = cfg.defs.iter().find(|d| d.name == agent).ok_or_else(|| {
            let names: Vec<&str> = cfg.defs.iter().map(|d| d.name.as_str()).collect();
            format!("no agent `{agent}`; known: {}", names.join(", "))
        })?;
        let parent = self.parent_model().unwrap_or_default();
        let mut model = child_model(a.model.or(def.model.clone()).as_deref(), parent, cfg.models.as_ref());
        if let Some(t) = &def.thinking
            && !model.contains(':')
        {
            model = format!("{model}:{t}");
        }
        let (mut tools, mcp, mut notes) =
            grant(def, a.tools, a.mcp, cfg.parent_picks, cfg.own.as_deref(), &cfg.servers);
        // Claude Code refuses an agent none of whose listed tools exists.
        if let (Some(wanted), Some(got)) = (&def.tools, &tools)
            && !wanted.is_empty()
            && got.is_empty()
            && mcp.is_empty()
        {
            return Err(format!(
                "agent `{agent}` would start with no tools: none of {} is available here",
                wanted.join(", ")
            ));
        }
        let short: String = a.task.chars().take(40).collect();
        // Claude `skills`: their text starts the task.
        let mut task = a.task;
        if !def.skills.is_empty() {
            let skills = novacode_core::resources::skills(&cfg.skill_dirs).0;
            let mut blocks = Vec::new();
            for s in &def.skills {
                let b = novacode_core::resources::expand(&format!("/skill:{s}"), &skills, &[]);
                if b.starts_with("<skill") {
                    blocks.push(b);
                } else {
                    notes.push(format!("skill `{s}` of agent `{agent}` not found"));
                }
            }
            blocks.push(task);
            task = blocks.join("\n\n");
        }
        // An active mode preset's tool list caps the child too.
        if let Some(only) = cfg.policy.overlay_tools() {
            let pool = tools.take().or_else(|| cfg.own.clone()).unwrap_or_default();
            tools = Some(
                pool.into_iter()
                    .filter(|t| only.iter().any(|o| o.eq_ignore_ascii_case(t)))
                    .collect(),
            );
        }
        let mut cwd = match &a.cwd {
            Some(c) => at(&cfg.cwd, c),
            None => cfg.cwd.clone(),
        };
        {
            let g = self.children.lock().unwrap();
            for d in &a.after {
                if !g.iter().any(|c| &c.n == d) {
                    return Err(format!("`after`: no agent {d}"));
                }
            }
        }
        let n = format!("a{}", self.next.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1);
        let title = a.title.unwrap_or(short);
        let read_only = def.read_only || cfg.policy.effective_mode() == novacode_core::policy::Mode::ReadOnly;
        if (a.worktree || def.worktree) && read_only {
            notes.push("no worktree: read-only (plan mode), so the subagent can't change files anyway".into());
        } else if a.worktree || def.worktree {
            // Git runs without the lock: the watchdog and event handling don't stall on it.
            let name = format!("{}-{n}", slug(&title));
            let (path, _) = novacode_tools::worktree::create_new(&cwd, &name)?;
            cwd = path;
        }
        let mut g = self.children.lock().unwrap();
        g.push(Child {
            n: n.clone(),
            title,
            agent,
            model,
            task,
            cwd,
            read_only: def.read_only,
            no_context: def.no_context,
            max_turns: def.max_turns,
            turns: 0,
            tools,
            mcp,
            notify: a.notify,
            after: a.after,
            status: Status::Queued,
            session: None,
            started: None,
            started_ms: 0,
            ended_ms: None,
            last_activity: Instant::now(),
            waits: Vec::new(),
            relays: Vec::new(),
            tx: None,
            pid: None,
            last_text: String::new(),
            report: None,
            log: VecDeque::new(),
            tokens: 0,
            tool_calls: 0,
            delivered: false,
            resume: None,
        });
        drop(g);
        self.start_ready();
        Ok((n, notes))
    }

    /// Start every queued child whose `after` agents are all done.
    fn start_ready(self: &Arc<Self>) {
        let cap = self.c().max_running;
        // Pick and mark in one lock: parallel spawns must not start a child twice.
        let ready: Vec<(String, String, Option<String>)> = {
            let mut g = self.children.lock().unwrap();
            let done: Vec<String> = g
                .iter()
                .filter(|c| c.status == Status::Done)
                .map(|c| c.n.clone())
                .collect();
            // Waiting on a question or approval still holds its slot.
            let running = g
                .iter()
                .filter(|c| !c.status.finished() && c.status != Status::Queued)
                .count();
            g.iter_mut()
                .filter(|c| c.status == Status::Queued && c.after.iter().all(|d| done.contains(d)))
                .take(cap.saturating_sub(running))
                .map(|c| {
                    c.status = Status::Starting;
                    if let Some(text) = c.resume.take() {
                        return (c.n.clone(), text, c.session.clone());
                    }
                    let first = format!(
                        "{}\"{}\" (agent {}, depth {}), started by another novacode agent. \
                         Do the task below. Your final message is your report to it: what you did, what you \
                         found with file paths, and anything left open. Write the report terse, for an agent, \
                         not a person: fragments, no preamble or pleasantries, no restating the task. Keep every \
                         fact the parent needs (paths, line numbers, names, numbers, commands, errors quoted \
                         exactly); drop only filler.\n\n{}",
                        novacode_core::session::SUBAGENT_TASK_PREFIX,
                        c.title,
                        c.agent,
                        self.place.depth + 1,
                        c.task
                    );
                    (c.n.clone(), first, None)
                })
                .collect()
        };
        for (n, prompt, session) in ready {
            let me = self.clone();
            tokio::spawn(async move { me.run(n, Some(prompt), session).await });
        }
        // Children waiting on one that failed can never start.
        let mut g = self.children.lock().unwrap();
        let failed: Vec<String> = g
            .iter()
            .filter(|c| matches!(c.status, Status::Failed | Status::Stopped))
            .map(|c| c.n.clone())
            .collect();
        for c in g.iter_mut().filter(|c| c.status == Status::Queued) {
            if let Some(d) = c.after.iter().find(|d| failed.contains(d)) {
                c.status = Status::Failed;
                c.report = Some(format!("not started: {d} did not finish"));
            }
        }
    }

    /// Run child `n` until it settles: start it (or resume `session`), send `prompt`.
    async fn run(self: Arc<Self>, n: String, prompt: Option<String>, session: Option<String>) {
        let Some((model, agent, cwd, title, tools, mcp, (read_only, no_context))) = self.with(&n, |c| {
            c.status = Status::Starting;
            c.turns = 0;
            c.started = Some(Instant::now());
            c.started_ms = novacode_core::session::now_ms();
            c.ended_ms = None;
            c.report = None;
            c.delivered = false;
            c.last_activity = Instant::now();
            c.waits.clear();
            (
                c.model.clone(),
                c.agent.clone(),
                c.cwd.clone(),
                c.title.clone(),
                c.tools.clone(),
                c.mcp.clone(),
                (c.read_only, c.no_context),
            )
        }) else {
            return;
        };
        let mut cmd = tokio::process::Command::new(&self.exe);
        cmd.arg("--rpc").args(["-m", &model]).args(["--agent", &agent]);
        // The parent's mode (read-only while plan mode is on), so a child can't do what it may not.
        let mode = if read_only {
            novacode_core::policy::Mode::ReadOnly
        } else {
            self.c().policy.effective_mode()
        };
        cmd.args(["--mode", mode.as_str()]);
        if no_context {
            cmd.arg("--no-context-files");
        }
        if let Some(t) = &tools {
            cmd.arg(format!("--tools={}", t.join(",")));
        }
        cmd.arg(format!("--mcp={}", mcp.join(",")));
        if self.c().trusted {
            cmd.arg("--trust");
        }
        if let Some(s) = &session {
            cmd.args(["-r", s]);
        }
        let mut chain = self.place.chain.clone();
        chain.push(self.me.clone());
        cmd.envs(&self.c().env);
        cmd.current_dir(&cwd)
            .env("NOVACODE_HOME", &self.home)
            .env("NOVACODE_AGENT_DEPTH", (self.place.depth + 1).to_string())
            .env("NOVACODE_AGENT_CHAIN", chain.join(","))
            .env("NOVACODE_AGENT_TITLE", &title)
            // The child's own tmux pane would be ours: keep it out of tmux options.
            .env_remove("TMUX_PANE")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true);
        let mut proc = match cmd.spawn() {
            Ok(p) => p,
            Err(e) => {
                self.finish(&n, Status::Failed, Some(format!("could not start: {e}")));
                return;
            }
        };
        let mut stdin = proc.stdin.take().expect("piped");
        let stdout = proc.stdout.take().expect("piped");
        let (tx, mut rx) = mpsc::unbounded_channel::<String>();
        let pid = proc.id();
        self.with(&n, |c| {
            c.tx = Some(tx.clone());
            c.pid = pid;
        });
        tokio::spawn(async move {
            while let Some(line) = rx.recv().await {
                if stdin.write_all(format!("{line}\n").as_bytes()).await.is_err() {
                    break;
                }
            }
            // Closing stdin ends an `--rpc` session.
        });
        if let Some(p) = prompt {
            let _ = tx.send(json!({"type": "prompt", "text": p}).to_string());
        }
        drop(tx);
        let mut lines = BufReader::new(stdout).lines();
        let mut prompted = false;
        while let Ok(Some(line)) = lines.next_line().await {
            let Ok(msg) = serde_json::from_str::<ServerMsg>(&line) else {
                continue;
            };
            match msg {
                ServerMsg::Hello { state, .. } => {
                    self.with(&n, |c| {
                        c.session = Some(state.session.clone());
                        c.status = Status::Running;
                    });
                }
                ServerMsg::Event { event } => {
                    if self.on_event(&n, event, &mut prompted) {
                        break;
                    }
                }
                ServerMsg::Ask { id, questions } => self.relay_ask(&n, &title, id, questions),
                ServerMsg::Approve {
                    id,
                    tool,
                    detail,
                    reason,
                    rule,
                    risky,
                } => self.relay_approval(&n, &title, id, tool, detail, reason, rule, risky),
                ServerMsg::Bye { .. } => break,
                _ => {}
            }
        }
        // Settled or gone: end it (drop stdin) and wait a moment.
        self.with(&n, |c| c.tx = None);
        let _ = tokio::time::timeout(Duration::from_secs(5), proc.wait()).await;
        let unfinished = self.with(&n, |c| !c.status.finished()).unwrap_or(false);
        if unfinished {
            self.finish(
                &n,
                Status::Failed,
                Some("the subagent exited before it finished".into()),
            );
        }
    }

    /// One child event. `true` = the child settled after our prompt.
    fn on_event(self: &Arc<Self>, n: &str, ev: AgentEvent, prompted: &mut bool) -> bool {
        match ev {
            // Claude `maxTurns`: past it, stop and hand over what it has as a partial report.
            AgentEvent::TurnStart => {
                let over = self
                    .with(n, |c| {
                        c.turns += 1;
                        c.max_turns
                            .filter(|m| c.turns > *m)
                            .map(|m| (m, c.tx.clone(), c.last_text.clone()))
                    })
                    .flatten();
                if let Some((max, tx, text)) = over {
                    if let Some(tx) = tx {
                        let _ = tx.send(json!({"type": "abort"}).to_string());
                    }
                    let report = format!("[partial: stopped at maxTurns {max}; send a message to resume]\n{text}");
                    self.finish(n, Status::Done, Some(report));
                    return true;
                }
            }
            AgentEvent::AgentStart => {
                *prompted = true;
                self.with(n, |c| {
                    c.status = Status::Running;
                    c.last_activity = Instant::now();
                });
            }
            AgentEvent::ToolStart { call } => {
                let (id, name, args) = (call.id, call.name, call.arguments);
                let waits = name == "agent" && args.get("action").and_then(Value::as_str) == Some("wait");
                let what = args
                    .get("path")
                    .or(args.get("command"))
                    .or(args.get("pattern"))
                    .and_then(Value::as_str)
                    .map(|s| s.chars().take(60).collect::<String>())
                    .unwrap_or_default();
                self.with(n, |c| {
                    c.last_activity = Instant::now();
                    if waits {
                        c.waits.push(id);
                    }
                    c.tool_calls += 1;
                    c.log.push_back(format!("{name} {what}").trim().to_string());
                    if c.log.len() > 200 {
                        c.log.pop_front();
                    }
                });
            }
            // Streaming and tool progress count as activity; no publish per event.
            AgentEvent::Stream(StreamEvent::Delta { .. }) | AgentEvent::ToolUpdate { .. } => {
                if let Some(c) = self.children.lock().unwrap().iter_mut().find(|c| c.n == n) {
                    c.last_activity = Instant::now();
                }
            }
            AgentEvent::ToolEnd { call_id, .. } => {
                if let Some(c) = self.children.lock().unwrap().iter_mut().find(|c| c.n == n) {
                    c.last_activity = Instant::now();
                    c.waits.retain(|w| *w != call_id);
                }
            }
            AgentEvent::Message(Message::Assistant(m)) => {
                let text = m.text();
                let failed = matches!(m.stop, StopReason::Error { .. });
                self.with(n, |c| {
                    c.last_activity = Instant::now();
                    c.tokens += m.usage.input + m.usage.output + m.usage.cache_read + m.usage.cache_write;
                    if !text.trim().is_empty() {
                        c.last_text = text;
                    }
                    if failed {
                        c.log
                            .push_back(format!("error: {}", m.error_message.clone().unwrap_or_default()));
                    }
                });
            }
            AgentEvent::Settled if *prompted => {
                let report = self.with(n, |c| c.last_text.clone()).unwrap_or_default();
                let status = if report.trim().is_empty() {
                    Status::Failed
                } else {
                    Status::Done
                };
                self.finish(n, status, Some(report));
                return true;
            }
            _ => {}
        }
        false
    }

    fn relay_ask(self: &Arc<Self>, n: &str, title: &str, id: u64, mut questions: Vec<novacode_types::Question>) {
        self.with(n, |c| c.status = Status::Waiting);
        for q in &mut questions {
            q.question = format!("[{title}] {}", q.question);
        }
        let (me, n2) = (self.clone(), n.to_string());
        let t = tokio::spawn(async move {
            let slot = me.c().slot.clone();
            let replies = slot.ask(questions).await;
            me.send_line(&n2, json!({"type": "answer", "ask": id, "replies": replies}));
            // Not if it was stopped or finished meanwhile.
            me.with(&n2, |c| {
                if c.status == Status::Waiting {
                    c.status = Status::Running;
                    // The wait for the user was no stall.
                    c.last_activity = Instant::now();
                }
            });
        });
        self.keep_relay(n, &t);
    }

    fn keep_relay(&self, n: &str, t: &tokio::task::JoinHandle<()>) {
        self.with(n, |c| {
            c.relays.retain(|r| !r.is_finished());
            c.relays.push(t.abort_handle());
        });
    }

    #[allow(clippy::too_many_arguments)]
    fn relay_approval(
        self: &Arc<Self>,
        n: &str,
        title: &str,
        id: u64,
        tool: String,
        detail: String,
        reason: String,
        rule: String,
        risky: bool,
    ) {
        self.with(n, |c| c.status = Status::Waiting);
        let key = if tool == "bash" { "command" } else { "path" };
        let req = ApprovalRequest {
            tool,
            args: json!({ key: detail }),
            reason: format!("subagent \"{title}\": {reason}"),
            rule,
            risky,
        };
        let (me, n2) = (self.clone(), n.to_string());
        let t = tokio::spawn(async move {
            let slot = me.c().slot.clone();
            let answer = match slot.approve(req).await {
                Approval::Once => json!("once"),
                Approval::Session => json!("session"),
                Approval::Always => json!("always"),
                Approval::Deny(why) => json!({"deny": why}),
            };
            me.send_line(&n2, json!({"type": "approval", "approval": id, "answer": answer}));
            me.with(&n2, |c| {
                if c.status == Status::Waiting {
                    c.status = Status::Running;
                    // The wait for the user was no stall.
                    c.last_activity = Instant::now();
                }
            });
        });
        self.keep_relay(n, &t);
    }

    fn send_line(&self, n: &str, v: Value) {
        if let Some(tx) = self.with(n, |c| c.tx.clone()).flatten() {
            let _ = tx.send(v.to_string());
        }
    }

    fn finish(self: &Arc<Self>, n: &str, status: Status, report: Option<String>) {
        let Some((title, report, notify, secs)) = self.with(n, |c| {
            c.status = status;
            c.ended_ms = Some(novacode_core::session::now_ms());
            c.report = report.or(c.report.take());
            c.tx = None;
            // Its open cards close in every window (the hub answers a dropped wait).
            for r in c.relays.drain(..) {
                r.abort();
            }
            (
                c.title.clone(),
                c.report.clone().unwrap_or_default(),
                c.notify,
                c.started.map_or(0, |t| t.elapsed().as_secs()),
            )
        }) else {
            return;
        };
        let notify = if self.c().unattended { Notify_::Wake } else { notify };
        let head = format!("subagent {n} \"{title}\" {} after {}", status.word(), dur(secs * 1000));
        if let Some(h) = self.handle() {
            let first = report.lines().find(|l| !l.trim().is_empty()).unwrap_or_default();
            h.emit(AgentEvent::Stream(StreamEvent::Notice {
                text: format!("⚑ {head}: {}", first.chars().take(120).collect::<String>()),
            }));
            // A `wait` in progress hands the report over itself.
            if notify != Notify_::Quiet && *self.waiting.lock().unwrap() == 0 {
                self.with(n, |c| c.delivered = true);
                h.remind_as(
                    "agent",
                    format!("{head}.\n{}", cut(&report, n)),
                    notify == Notify_::Wake,
                );
            }
        }
        self.start_ready();
    }

    /// Children not finished yet (print mode waits for them).
    pub fn pending(&self) -> usize {
        self.children
            .lock()
            .unwrap()
            .iter()
            .filter(|c| !c.status.finished())
            .count()
    }

    fn stop(self: &Arc<Self>, n: &str, why: &str) -> bool {
        let Some((tx, pid, done)) = self.with(n, |c| (c.tx.take(), c.pid, c.status.finished())) else {
            return false;
        };
        if done {
            return true;
        }
        if let Some(tx) = tx {
            let _ = tx.send(json!({"type": "abort"}).to_string());
        }
        self.finish(n, Status::Stopped, Some(format!("stopped: {why}")));
        // Stdin is closed; make sure it goes.
        if let Some(pid) = pid {
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_secs(3)).await;
                if alive(pid) {
                    let _ = std::process::Command::new("kill").arg(pid.to_string()).status();
                }
            });
        }
        true
    }

    /// Message a child: steer a running one, resume a finished one.
    fn send(self: &Arc<Self>, n: &str, text: String) -> Result<String, String> {
        let (status, session, tx) = self
            .with(n, |c| (c.status, c.session.clone(), c.tx.clone()))
            .ok_or(format!("no agent {n}"))?;
        if !status.finished() && status != Status::Queued {
            let tx = tx.ok_or("the agent is starting; try again")?;
            let _ = tx.send(json!({"type": "steer", "text": text}).to_string());
            return Ok(format!("sent to {n}; it sees the message before its next step"));
        }
        if session.is_none() {
            return Err(format!("{n} never started; nothing to resume"));
        }
        // Resumes take a slot like new spawns: queue it, then start what fits.
        self.with(n, |c| {
            c.status = Status::Queued;
            c.after.clear();
            c.delivered = false;
            c.resume = Some(match c.resume.take() {
                Some(earlier) => format!("{earlier}\n\n{text}"),
                None => text,
            });
        });
        self.start_ready();
        Ok(match self.with(n, |c| c.status) {
            Some(Status::Queued) => format!("{n} is queued; it resumes with the message when a running subagent ends"),
            _ => format!("resumed {n} with the message"),
        })
    }

    fn line(c: &Child) -> String {
        let time = match (c.started, c.ended_ms) {
            (Some(_), Some(e)) => dur(e.saturating_sub(c.started_ms)),
            (Some(t), None) => dur(t.elapsed().as_millis() as u64),
            _ => "-".into(),
        };
        format!(
            "{} {:<8} {} \"{}\" ({}, {time}, {}k tokens)",
            c.n,
            c.status.word(),
            c.agent,
            c.title,
            c.model,
            c.tokens / 1000
        )
    }

    /// Block until the given agents (default: all started) finish, or `timeout`.
    async fn wait(&self, ids: Vec<String>, any: bool, timeout: Duration) -> String {
        *self.waiting.lock().unwrap() += 1;
        let deadline = tokio::time::Instant::now() + timeout;
        let out = loop {
            let notified = self.changed.notified();
            let (done, pending): (Vec<String>, usize) = {
                let mut g = self.children.lock().unwrap();
                let pick: Vec<&mut Child> = g
                    .iter_mut()
                    .filter(|c| {
                        if ids.is_empty() {
                            !c.delivered
                        } else {
                            ids.contains(&c.n)
                        }
                    })
                    .collect();
                let pending = pick.iter().filter(|c| !c.status.finished()).count();
                let total = pick.len();
                let ready = total > 0 && (pending == 0 || (any && pending < total));
                if !ready && tokio::time::Instant::now() < deadline {
                    (Vec::new(), pending)
                } else {
                    let mut v = Vec::new();
                    for c in pick.into_iter().filter(|c| c.status.finished() && !c.delivered) {
                        c.delivered = true;
                        v.push(format!(
                            "{}\n{}",
                            Self::line(c),
                            cut(c.report.as_deref().unwrap_or_default(), &c.n)
                        ));
                    }
                    if v.is_empty() && total == 0 {
                        break "no subagents to wait for".to_string();
                    }
                    break if v.is_empty() {
                        format!("timed out; {pending} still running")
                    } else {
                        let mut s = v.join("\n\n");
                        if pending > 0 {
                            s.push_str(&format!("\n\n({pending} still running)"));
                        }
                        s
                    };
                }
            };
            let _ = (done, pending);
            tokio::select! {
                _ = notified => {}
                _ = tokio::time::sleep_until(deadline) => {}
            }
        };
        *self.waiting.lock().unwrap() -= 1;
        out
    }
}

fn dur(ms: u64) -> String {
    let s = ms / 1000;
    match s {
        0..60 => format!("{s}s"),
        60..3600 => format!("{}m{:02}s", s / 60, s % 60),
        _ => format!("{}h{:02}m", s / 3600, s % 3600 / 60),
    }
}

fn at(cwd: &Path, p: &str) -> PathBuf {
    match p.strip_prefix("~/") {
        Some(rest) => novacode_base::platform::home().unwrap_or_default().join(rest),
        None => cwd.join(p),
    }
}

fn cut(report: &str, n: &str) -> String {
    if report.chars().count() <= REPORT_CUT {
        return report.to_string();
    }
    let head: String = report.chars().take(REPORT_CUT).collect();
    format!("{head}…\n[cut; full report: agent read {n}]")
}

fn slug(s: &str) -> String {
    let s: String = s
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let s: Vec<&str> = s.split('-').filter(|p| !p.is_empty()).take(4).collect();
    if s.is_empty() { "agent".into() } else { s.join("-") }
}

// ---------------------------------------------------------------------------
// The tool

#[derive(Deserialize)]
struct SpawnArgs {
    task: String,
    title: Option<String>,
    agent: Option<String>,
    model: Option<String>,
    cwd: Option<String>,
    #[serde(default)]
    after: Vec<String>,
    #[serde(default)]
    notify: Notify_,
    #[serde(default)]
    worktree: bool,
    tools: Option<Vec<String>>,
    mcp: Option<Vec<String>>,
}

pub struct AgentTool {
    pub sup: Arc<Supervisor>,
    description: String,
}

impl AgentTool {
    pub fn new(sup: Arc<Supervisor>) -> Self {
        let cfg = sup.c();
        let agents: Vec<String> = cfg
            .defs
            .iter()
            .map(|d| {
                let mut kit = d.tools.as_ref().map_or("all your tools".into(), |t| t.join(", "));
                if !d.mcp.is_empty() {
                    kit += &format!("; mcp: {}", d.mcp.join(", "));
                }
                format!("- {} [{kit}]: {}", d.name, d.description)
            })
            .collect();
        let (pick, tools) = if cfg.parent_picks {
            let mcp = if cfg.servers.is_empty() {
                String::new()
            } else {
                format!(
                    " MCP servers are off unless you grant them with `mcp` (it calls them through codemode; \
                     say in the task which server tool to use first): {}.",
                    cfg.servers.join(", ")
                )
            };
            (
                ", tools?: [names], mcp?: [servers]",
                format!(
                    "A subagent sees only its tools: its type's (in brackets below), or `tools` picked from \
                     your own. Give what the task needs (one that must build or test needs bash).{mcp}"
                ),
            )
        } else {
            (
                "",
                "A subagent sees only its type's tools (in brackets below).".to_string(),
            )
        };
        AgentTool {
            description: format!(
                "Run subagents: separate novacode sessions that work in the background and report back.\n\
                 Actions: spawn {{task, title?, agent?, model?, cwd?, after?: [ids], notify?: wake|next|quiet, worktree?{pick}}} → id; \
                 list; status {{id}}; log {{id, limit?}}; read {{id, offset?, limit?}} (full report); \
                 send {{id, message}} (steers a running agent, resumes a finished one); \
                 wait {{ids?, any?, timeout?}} (blocks until reports arrive, default all, 600 s); stop {{id}}.\n\
                 Reports arrive by themselves (notify wake starts a turn); don't poll. \
                 A subagent's edit/write claims the file: other agents, you included, are refused on it (you claim none; bash isn't checked).\n{tools}\nAgents:\n{}",
                agents.join("\n")
            ),
            sup,
        }
    }
}

impl Tool for AgentTool {
    fn name(&self) -> &str {
        "agent"
    }
    fn description(&self) -> &str {
        &self.description
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": {"type": "string", "enum": ["spawn", "list", "status", "log", "read", "send", "wait", "stop"]},
                "id": {"type": "string", "description": "Agent id (a1, a2…)."},
                "ids": {"type": "array", "items": {"type": "string"}},
                "task": {"type": "string", "description": "spawn: complete, self-contained instructions."},
                "title": {"type": "string", "description": "spawn: purpose in a few words."},
                "agent": {"type": "string", "description": "spawn: agent type (default worker)."},
                "model": {"type": "string", "description": "spawn: provider/model[:level]."},
                "cwd": {"type": "string"},
                "after": {"type": "array", "items": {"type": "string"}, "description": "spawn: start when these agents are done."},
                "notify": {"type": "string", "enum": ["wake", "next", "quiet"]},
                "worktree": {"type": "boolean", "description": "spawn: work in a new git worktree (.worktrees/); you merge its branch."},
                "tools": {"type": "array", "items": {"type": "string"}, "description": "spawn: its tools, from yours (default: its type's)."},
                "mcp": {"type": "array", "items": {"type": "string"}, "description": "spawn: MCP servers it may use (default: none)."},
                "message": {"type": "string"},
                "any": {"type": "boolean"},
                "timeout": {"type": "number", "description": "wait: seconds (default 600)."},
                "offset": {"type": "number"},
                "limit": {"type": "number"}
            },
            "required": ["action"]
        })
    }
    fn sequential(&self) -> bool {
        false
    }
    fn large_output(&self) -> bool {
        false
    }
    fn guidelines(&self) -> &[&str] {
        &[
            "Delegate independent work to subagents with `agent` spawn and keep working; give each a complete task, since they don't see this conversation.",
        ]
    }
    fn execute<'a>(&'a self, args: Value, ctx: &'a ToolCtx) -> BoxFuture<'a, ToolOutput> {
        Box::pin(async move {
            let s = &self.sup;
            let id = args["id"].as_str().unwrap_or_default().to_string();
            let missing = || ToolOutput::error(format!("no agent `{id}`; `agent list` shows them"));
            match args["action"].as_str().unwrap_or_default() {
                "spawn" => match serde_json::from_value::<SpawnArgs>(args.clone()) {
                    Ok(a) => match s.spawn(a) {
                        Ok((n, notes)) => ToolOutput {
                            details: Some(serde_json::json!({ "id": n })),
                            ..ToolOutput::text(
                                std::iter::once(format!("started {n}; its report arrives when it's done"))
                                    .chain(notes)
                                    .collect::<Vec<_>>()
                                    .join("\n"),
                            )
                        },
                        Err(e) => ToolOutput::error(e),
                    },
                    Err(e) => ToolOutput::error(format!("spawn: {e}")),
                },
                "list" => {
                    let g = s.children.lock().unwrap();
                    ToolOutput::text(if g.is_empty() {
                        "no subagents".to_string()
                    } else {
                        g.iter().map(Supervisor::line).collect::<Vec<_>>().join("\n")
                    })
                }
                "status" => match s.with(&id, |c| {
                    let mut t = Supervisor::line(c);
                    if let Some(sess) = &c.session {
                        t += &format!(
                            "\nsession {sess} (watch: novacode attach {})",
                            &sess[..sess.len().min(13)]
                        );
                    }
                    t += &format!("\ncwd {}", c.cwd.display());
                    if let Some(l) = c.log.back() {
                        t += &format!("\nlast: {l}");
                    }
                    t
                }) {
                    Some(t) => ToolOutput::text(t),
                    None => missing(),
                },
                "log" => {
                    let limit = args["limit"].as_u64().unwrap_or(20) as usize;
                    match s.with(&id, |c| {
                        let skip = c.log.len().saturating_sub(limit);
                        c.log.iter().skip(skip).cloned().collect::<Vec<_>>().join("\n")
                    }) {
                        Some(t) if t.is_empty() => ToolOutput::text("(no activity yet)"),
                        Some(t) => ToolOutput::text(t),
                        None => missing(),
                    }
                }
                "read" => {
                    let off = args["offset"].as_u64().unwrap_or(0) as usize;
                    let limit = args["limit"].as_u64().unwrap_or(20_000) as usize;
                    match s.with(&id, |c| {
                        c.delivered = true;
                        c.report.clone()
                    }) {
                        Some(Some(r)) => ToolOutput::text(r.chars().skip(off).take(limit).collect::<String>()),
                        Some(None) => ToolOutput::text("no report yet; it's still working"),
                        None => missing(),
                    }
                }
                "send" => {
                    let msg = args["message"].as_str().unwrap_or_default().to_string();
                    match s.send(&id, msg) {
                        Ok(t) => ToolOutput::text(t),
                        Err(e) => ToolOutput::error(e),
                    }
                }
                "wait" => {
                    let ids: Vec<String> = args["ids"]
                        .as_array()
                        .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
                        .or_else(|| (!id.is_empty()).then(|| vec![id.clone()]))
                        .unwrap_or_default();
                    let secs = args["timeout"].as_f64().unwrap_or(600.0).clamp(1.0, 3600.0);
                    let any = args["any"].as_bool().unwrap_or(false);
                    tokio::select! {
                        t = s.wait(ids, any, Duration::from_secs_f64(secs)) => ToolOutput::text(t),
                        _ = ctx.cancel.cancelled() => ToolOutput::error("wait cancelled"),
                    }
                }
                "stop" => {
                    if s.stop(&id, "by the parent") {
                        ToolOutput::text(format!("stopped {id}"))
                    } else {
                        missing()
                    }
                }
                other => ToolOutput::error(format!("unknown action `{other}`")),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fake `novacode --rpc`: hello, then per prompt a reply that echoes it and settles.
    #[cfg(unix)]
    fn fake_child(dir: &Path) -> PathBuf {
        let hello = serde_json::to_string(&ServerMsg::Hello {
            protocol: 1,
            version: "t".into(),
            capabilities: vec![],
            state: crate::wire::State {
                session: "SESSION".into(),
                ..Default::default()
            },
        })
        .unwrap();
        let ev = |e: AgentEvent| serde_json::to_string(&ServerMsg::Event { event: e }).unwrap();
        let reply = ev(AgentEvent::Message(Message::Assistant(
            novacode_types::AssistantMessage {
                content: vec![novacode_types::Block::text("REPLY")],
                stop: StopReason::Stop,
                usage: Default::default(),
                provider: "p".into(),
                model: "m".into(),
                response_id: None,
                error_message: None,
                thinking_level: None,
            },
        )));
        let script = dir.join("child.py");
        let lines = [
            "#!/usr/bin/env python3".to_string(),
            "import sys, json, os, uuid, time".into(),
            // Every session starts like a child name, so a name lookup that matched session
            // prefixes first would pick the wrong child (it did, 1 run in ~30).
            format!("print({hello:?}.replace('SESSION', 'a2' + str(uuid.uuid4())), flush=True)"),
            "for line in sys.stdin:".into(),
            "    m = json.loads(line)".into(),
            "    if m['type'] != 'prompt': continue".into(),
            "    time.sleep(0.3)".into(),
            format!("    print({:?}, flush=True)", ev(AgentEvent::AgentStart)),
            format!(
                "    print({reply:?}.replace('REPLY', os.getcwd() + ' :: ' + m['text'].split(chr(10))[-1] + os.environ.get('NC_ENV', '') + ' ARGS ' + ' '.join(sys.argv[1:])), flush=True)"
            ),
            format!("    print({:?}, flush=True)", ev(AgentEvent::Settled)),
        ];
        std::fs::write(&script, lines.join("\n")).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        script
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn spawn_after_wait_and_send() {
        let tmp = std::env::temp_dir().join(format!("nc-sup-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        let sup = Supervisor::new(Setup {
            slot: HubSlot::default(),
            unattended: false,
            me: "ROOT".into(),
            home: tmp.clone(),
            cwd: tmp.clone(),
            trusted: false,
            defs: defs(&[]).0,
            parent_picks: true,
            servers: vec![],
            stall_min: 10,
            timeout_min: 45,
            models: Arc::new(|_: &str, _: &str| None),
            skill_dirs: vec![],
            max_running: 4,
            exe: Some(fake_child(&tmp)),
            nerd: false,
            env: [("NC_ENV".to_string(), " env-set".to_string())].into(),
            policy: Arc::new(novacode_core::policy::Policy::new(
                Default::default(),
                tmp.clone(),
                None,
            )),
        });
        let spawn = |task: &str, after: &[&str]| {
            sup.spawn(serde_json::from_value(json!({"task": task, "after": after, "model": "p/m"})).unwrap())
                .unwrap()
                .0
        };
        // Three quick spawns: each starts exactly once; c waits for a.
        let (a, b) = (spawn("task A", &[]), spawn("task B", &[]));
        let c = spawn("task C", &[&a]);
        assert_eq!(sup.with(&c, |c| c.status), Some(Status::Queued));
        let out = sup.wait(vec![], false, Duration::from_secs(20)).await;
        assert!(
            out.contains("task A") && out.contains("task B") && out.contains("task C"),
            "{out}"
        );
        assert_eq!(out.matches(":: task").count(), 3, "one report each: {out}");
        assert_eq!(
            out.matches(" env-set").count(),
            3,
            "`agents.env` reaches every child: {out}"
        );
        assert_eq!(sup.children.lock().unwrap().len(), 3);
        let started: Vec<u64> = ["a1", "a2", "a3"]
            .iter()
            .map(|n| sup.with(n, |c| c.started_ms).unwrap())
            .collect();
        assert!(
            started[2] >= sup.with(&a, |c| c.ended_ms.unwrap()).unwrap(),
            "c started after a ended"
        );
        let _ = b;
        // Nothing left to wait for.
        assert_eq!(
            sup.wait(vec![], false, Duration::from_secs(1)).await,
            "no subagents to wait for"
        );
        // Send to a finished agent resumes it with the message.
        sup.send(&a, "again".into()).unwrap();
        let out = sup.wait(vec![a.clone()], false, Duration::from_secs(20)).await;
        assert!(out.contains(":: again"), "{out}");
        assert!(out.contains("--mode auto"), "children get the parent's mode: {out}");
        // Plan mode on: no worktree is made (it can't change files anyway).
        sup.c().policy.set_overlay(Some(("plan".into(), true, None)));
        let (w, notes) = sup
            .spawn(serde_json::from_value(json!({"task": "wt", "worktree": true, "model": "p/m"})).unwrap())
            .unwrap();
        assert!(notes.iter().any(|n| n.starts_with("no worktree")), "{notes:?}");
        assert_eq!(sup.with(&w, |c| c.cwd.clone()).unwrap(), tmp);
        sup.wait(vec![w], false, Duration::from_secs(20)).await;
        // Plan mode on: a new child starts read-only, with only the preset's tools.
        sup.set_own_tools(vec!["read".into(), "bash".into()]);
        sup.c()
            .policy
            .set_overlay(Some(("plan".into(), true, Some(vec!["read".into()]))));
        let d = spawn("task D", &[]);
        let out = sup.wait(vec![d], false, Duration::from_secs(20)).await;
        assert!(
            out.contains("--mode read-only") && out.contains("--tools=read "),
            "{out}"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn at_most_max_running_children_and_slots_free_on_exit() {
        let tmp = std::env::temp_dir().join(format!("nc-cap-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        let setup = |exe: PathBuf| Setup {
            slot: HubSlot::default(),
            unattended: false,
            me: "ROOT".into(),
            home: tmp.clone(),
            cwd: tmp.clone(),
            trusted: false,
            defs: defs(&[]).0,
            parent_picks: true,
            servers: vec![],
            stall_min: 10,
            timeout_min: 45,
            models: Arc::new(|_: &str, _: &str| None),
            skill_dirs: vec![],
            max_running: 2,
            exe: Some(exe),
            nerd: false,
            env: Default::default(),
            policy: Arc::new(novacode_core::policy::Policy::new(
                Default::default(),
                tmp.clone(),
                None,
            )),
        };
        let spawn = |sup: &Arc<Supervisor>, task: &str| {
            sup.spawn(serde_json::from_value(json!({"task": task, "model": "p/m"})).unwrap())
                .unwrap()
                .0
        };
        let sup = Supervisor::new(setup(fake_child(&tmp)));
        let ids: Vec<String> = (0..5).map(|i| spawn(&sup, &format!("t{i}"))).collect();
        // A queued child can be stopped before it ever starts.
        assert!(sup.stop(&ids[4], "not needed"));
        let mut most = 0;
        while sup.pending() > 0 {
            let running = sup
                .children
                .lock()
                .unwrap()
                .iter()
                .filter(|c| matches!(c.status, Status::Starting | Status::Running))
                .count();
            most = most.max(running);
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(most, 2, "never more than max_running at once");
        {
            let g = sup.children.lock().unwrap();
            assert!(g[..4].iter().all(|c| c.status == Status::Done), "all ran");
            assert!(g[4].status == Status::Stopped && g[4].started.is_none());
            // Spawn order: t2 started only after one of t0/t1 ended.
            let first_end = g[..2].iter().map(|c| c.ended_ms.unwrap()).min().unwrap();
            assert!(g[2].started_ms >= first_end);
        }
        // A child that dies at once frees its slot, so the rest still run.
        let sup = Supervisor::new(setup("/bin/false".into()));
        for i in 0..3 {
            spawn(&sup, &format!("crash{i}"));
        }
        let out = sup.wait(vec![], false, Duration::from_secs(20)).await;
        assert_eq!(out.matches("exited before it finished").count(), 3, "{out}");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[cfg(unix)]
    fn test_setup(tmp: &Path, exe: PathBuf) -> Setup {
        Setup {
            slot: HubSlot::default(),
            unattended: false,
            me: "ROOT".into(),
            home: tmp.to_path_buf(),
            cwd: tmp.to_path_buf(),
            trusted: false,
            defs: defs(&[]).0,
            parent_picks: true,
            servers: vec![],
            stall_min: 10,
            timeout_min: 45,
            models: Arc::new(|_: &str, _: &str| None),
            skill_dirs: vec![],
            max_running: 4,
            exe: Some(exe),
            nerd: false,
            env: Default::default(),
            policy: Arc::new(novacode_core::policy::Policy::new(
                Default::default(),
                tmp.to_path_buf(),
                None,
            )),
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn tool_progress_agent_wait_and_relays_count_as_activity() {
        let tmp = std::env::temp_dir().join(format!("nc-act-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        // A child that says nothing: every event below comes from the test.
        let exe = tmp.join("idle.py");
        std::fs::write(&exe, "#!/usr/bin/env python3\nimport sys\nfor _ in sys.stdin: pass\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
        let sup = Supervisor::new(test_setup(&tmp, exe));
        {
            let mut g = sup.cfg.write().unwrap();
            let mut c = Cfg::clone(&g);
            c.stall = Duration::from_secs(1);
            *g = Arc::new(c);
        }
        let n = sup
            .spawn(serde_json::from_value(json!({"task": "t", "model": "p/m"})).unwrap())
            .unwrap()
            .0;
        // Let its run task start (it sets Starting) before the events below.
        tokio::time::sleep(Duration::from_millis(300)).await;
        let idle = || {
            sup.with(&n, |c| {
                c.last_activity = Instant::now().checked_sub(Duration::from_secs(60)).unwrap()
            });
        };
        let fresh = || {
            sup.with(&n, |c| c.last_activity.elapsed() < Duration::from_secs(5))
                .unwrap()
        };
        let stalled = || sup.overdue().iter().any(|(m, _)| *m == n);
        let mut p = false;
        idle();
        assert!(stalled());
        let call =
            serde_json::from_value(json!({"id": "w1", "name": "agent", "arguments": {"action": "wait"}})).unwrap();
        sup.on_event(&n, AgentEvent::ToolStart { call }, &mut p);
        idle();
        assert!(!stalled(), "`agent wait` is exempt from the stall check");
        sup.on_event(
            &n,
            AgentEvent::ToolUpdate {
                call_id: "w1".into(),
                text: "x".into(),
            },
            &mut p,
        );
        assert!(fresh(), "ToolUpdate is activity");
        idle();
        sup.on_event(
            &n,
            AgentEvent::ToolEnd {
                call_id: "w1".into(),
                is_error: false,
                duration_ms: 1,
            },
            &mut p,
        );
        assert!(fresh(), "ToolEnd is activity");
        idle();
        assert!(stalled(), "the exemption ends with the wait");
        // No window: the approval is refused at once, and the child is back to work, active.
        sup.relay_approval(&n, "t", 1, "bash".into(), "ls".into(), "r".into(), "".into(), false);
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(sup.with(&n, |c| c.status), Some(Status::Running));
        assert!(fresh(), "answering a relay is activity");
        sup.stop(&n, "done");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn worktree_names_never_repeat_across_sessions() {
        let tmp = std::env::temp_dir().join(format!("nc-wt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        let git = |args: &[&str]| {
            assert!(
                std::process::Command::new("git")
                    .args(["-c", "user.name=t", "-c", "user.email=t@t"])
                    .args(args)
                    .current_dir(&tmp)
                    .output()
                    .unwrap()
                    .status
                    .success()
            )
        };
        git(&["init", "-q"]);
        git(&["commit", "-q", "--allow-empty", "-m", "i"]);
        let exe = fake_child(&tmp);
        let mut paths = Vec::new();
        // Two sessions: both number their first child a1.
        for _ in 0..2 {
            let sup = Supervisor::new(test_setup(&tmp, exe.clone()));
            let n = sup
                .spawn(
                    serde_json::from_value(json!({"task": "t", "title": "same", "worktree": true, "model": "p/m"}))
                        .unwrap(),
                )
                .unwrap()
                .0;
            assert_eq!(n, "a1");
            paths.push(sup.with(&n, |c| c.cwd.clone()).unwrap());
            sup.wait(vec![n], false, Duration::from_secs(20)).await;
        }
        assert_eq!(paths[0], tmp.join(".worktrees/same-a1"));
        assert_eq!(paths[1], tmp.join(".worktrees/same-a1-2"));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn small_model_follows_the_parents_provider() {
        let small = |p: &str, w: &str| match (p, w) {
            ("claude", "small") => Some("claude/haiku".to_string()),
            ("codex", "small") => Some("codex/mini".to_string()),
            ("claude", "sonnet") => Some("claude/claude-sonnet-5".to_string()),
            _ => None,
        };
        // A Claude family name resolves on the parent's provider, when it has it.
        assert_eq!(
            child_model(Some("sonnet"), "claude/opus".into(), &small),
            "claude/claude-sonnet-5"
        );
        assert_eq!(child_model(Some("sonnet"), "codex/gpt".into(), &small), "codex/gpt");
        assert_eq!(child_model(Some("small"), "claude/opus".into(), &small), "claude/haiku");
        // After `/model` switched the parent to another provider, scouts follow it.
        assert_eq!(child_model(Some("small"), "codex/gpt".into(), &small), "codex/mini");
        assert_eq!(
            child_model(Some("small"), "local/llama".into(), &small),
            "local/llama",
            "no small model: the parent's"
        );
        assert_eq!(
            child_model(Some("x/y"), "codex/gpt".into(), &small),
            "x/y",
            "explicit wins"
        );
        assert_eq!(child_model(None, "codex/gpt".into(), &small), "codex/gpt");
    }

    /// L7.16: Claude Code agent files map onto novacode's subagents, never widening them.
    #[test]
    fn claude_agent_fields() {
        let dir = std::env::temp_dir().join(format!("nc-claude-agents-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let w = |name: &str, text: &str| std::fs::write(dir.join(format!("{name}.md")), text).unwrap();
        w(
            "full",
            "---\nname: full\ndescription: d\ntools:\n  - Read\n  - Bash\n  - mcp__github__get_issue\ndisallowedTools: [Bash, mcp__context7]\nmodel: inherit\npermissionMode: plan\nmaxTurns: 3\nskills: [lint]\nisolation: worktree\neffort: high\nomitClaudeMd: true\nmcpServers: [context7]\nmemory: user\n---\nRole.",
        );
        w(
            "wide",
            "---\nname: wide\ndescription: d\npermissionMode: bypassPermissions\nmodel: sonnet\n---\nx",
        );
        w(
            "hooked",
            "---\nname: hooked\ndescription: d\nhooks:\n  PreToolUse: []\n---\nx",
        );
        let (defs, notes) = defs(std::slice::from_ref(&dir));
        let _ = std::fs::remove_dir_all(&dir);
        let by = |n: &str| defs.iter().find(|d| d.name == n).cloned();
        let full = by("full").unwrap();
        assert_eq!(full.tools.as_deref(), Some(&["read".to_string(), "bash".into()][..]));
        assert_eq!(full.mcp, ["context7", "github"]);
        assert_eq!(full.disallowed, ["bash", "mcp__context7"]);
        assert_eq!(full.model, None, "inherit = the parent's");
        assert!(full.read_only && full.worktree && full.no_context);
        assert_eq!((full.max_turns, full.thinking.as_deref()), (Some(3), Some("high")));
        assert_eq!(full.skills, ["lint"]);
        let wide = by("wide").unwrap();
        assert!(!wide.read_only);
        assert_eq!(wide.model.as_deref(), Some("sonnet"));
        assert!(by("hooked").is_none(), "an agent whose hooks can't run is not loaded");
        let all = notes.join("\n");
        assert!(all.contains("`hooked`: not loaded"), "{all}");
        assert!(
            all.contains("`wide`: `permissionMode: bypassPermissions` ignored"),
            "{all}"
        );
        assert!(all.contains("`full`: not supported, ignored: memory"), "{all}");
        // Disallowed tools and servers are never granted, whoever picks.
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        let own = s(&["read", "bash", "edit", "codemode"]);
        let (t, m, _) = grant(
            &full,
            Some(s(&["read", "bash", "edit"])),
            None,
            true,
            Some(&own),
            &s(&["github", "context7"]),
        );
        assert_eq!(t.unwrap(), ["read", "edit", "codemode"]);
        assert_eq!(m, ["github"]);
    }

    #[test]
    fn child_tools_from_defaults_settings_and_picks() {
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        let mut d = defs(&[]).0;
        configure(&mut d, |n| {
            (n == "reviewer").then(|| AgentCfg {
                tools: Some(s(&["Read", "bash"])),
                mcp: Some(s(&["codegraph"])),
            })
        });
        let by = |n: &str| d.iter().find(|x| x.name == n).unwrap().clone();
        let own = s(&["read", "grep", "bash", "edit", "codemode", "web_fetch"]);
        let servers = s(&["codegraph", "context7"]);
        let g = |def: &Def, t: Option<&[&str]>, m: Option<&[&str]>, picks: bool| {
            grant(def, t.map(s), m.map(s), picks, Some(&own), &servers)
        };
        // Type defaults, bounded by the parent's own tools (scout's find/ls/todo aren't there).
        let (t, m, notes) = g(&by("scout"), None, None, true);
        assert_eq!((t, m, notes.len()), (Some(s(&["read", "grep"])), vec![], 0));
        // No list: everything the parent has.
        assert_eq!(g(&by("worker"), None, None, true).0, Some(own.clone()));
        // Settings replace the defaults (Claude names mapped) and grant MCP, which adds codemode.
        let (t, m, notes) = g(&by("reviewer"), None, None, true);
        assert_eq!(t, Some(s(&["read", "bash", "codemode"])));
        assert_eq!((m, notes.len()), (s(&["codegraph"]), 1));
        // The parent widens beyond the type, never beyond itself; unknown names are noted.
        let (t, m, notes) = g(
            &by("scout"),
            Some(&["read", "edit", "write", "x__y"]),
            Some(&["nope"]),
            true,
        );
        assert_eq!((t, m), (Some(s(&["read", "edit"])), vec![]));
        assert!(
            notes[0].contains("write, x__y") && notes[0].contains("mcp"),
            "{notes:?}"
        );
        assert!(notes[1].contains("no MCP server nope"), "{notes:?}");
        // The user turned picking off: defaults, and the parent is told why.
        let (t, _, notes) = g(&by("scout"), Some(&["edit"]), None, false);
        assert_eq!(t, Some(s(&["read", "grep"])));
        assert!(notes[0].contains("parentPicksTools"), "{notes:?}");
    }

    #[cfg(unix)]
    #[test]
    fn defs_and_claims() {
        let tmp = std::env::temp_dir().join(format!("nc-agents-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("agents")).unwrap();
        std::fs::write(
            tmp.join("agents/docs.md"),
            "---\nname: docs\ndescription: writes docs\ntools: read, write\nmode: replace\n---\nYou write docs.",
        )
        .unwrap();
        // A Claude Code agent file: its tool names and `model: inherit` are mapped.
        std::fs::create_dir_all(tmp.join("claude")).unwrap();
        std::fs::write(
            tmp.join("claude/rev.md"),
            "---\nname: rev\ndescription: reviews\ntools: [Read, \"Grep\", Glob]\nmodel: sonnet\n---\nReview.",
        )
        .unwrap();
        // Same file through a symlink: once, silently. Same name elsewhere: first wins, reported.
        std::os::unix::fs::symlink(tmp.join("claude/rev.md"), tmp.join("agents/rev.md")).unwrap();
        std::fs::write(tmp.join("claude/docs.md"), "---\ndescription: other docs\n---\nx").unwrap();
        let (d, shadowed) = defs(&[tmp.join("agents"), tmp.join("claude")]);
        assert_eq!(shadowed.len(), 1, "{shadowed:?}");
        assert!(shadowed[0].contains("claude/docs.md"), "{shadowed:?}");
        let rev = d.iter().find(|d| d.name == "rev").unwrap();
        assert_eq!(
            rev.tools.as_deref(),
            Some(&["read".to_string(), "grep".to_string(), "find".to_string()][..])
        );
        assert_eq!(
            rev.model.as_deref(),
            Some("sonnet"),
            "a family name, resolved on the parent's provider at spawn"
        );
        let docs = d.iter().find(|d| d.name == "docs").unwrap();
        assert_eq!(
            docs.tools.as_deref(),
            Some(&["read".to_string(), "write".to_string()][..])
        );
        assert!(docs.replace && docs.prompt == "You write docs.");
        assert!(d.iter().any(|d| d.name == "scout"));

        // Two children of root R: the first to edit claims, the second is refused;
        // a grandchild of the holder inherits; main is refused but never claims.
        std::fs::write(tmp.join("f.rs"), "x").unwrap();
        let place = |depth, chain: &[&str]| Place {
            depth,
            chain: chain.iter().map(|s| s.to_string()).collect(),
        };
        let a = Claims::new(&tmp, "A", place(1, &["R"]), &tmp);
        let b = Claims::new(&tmp, "B", place(1, &["R"]), &tmp);
        let a1 = Claims::new(&tmp, "A1", place(2, &["R", "A"]), &tmp);
        let main = Claims::new(&tmp, "R", place(0, &[]), &tmp);
        let edit = json!({"path": "f.rs"});
        assert!(a.check(&a.paths("edit", &edit)).is_ok());
        let e = b.check(&b.paths("edit", &edit)).unwrap_err();
        assert!(e.contains("being edited by subagent"), "{e}");
        assert!(a1.check(&a1.paths("edit", &edit)).is_ok(), "claims pass down");
        assert!(main.check(&main.paths("edit", &edit)).is_err());
        assert!(main.check(&main.paths("write", &json!({"path": "new.rs"}))).is_ok());
        assert!(
            b.check(&b.paths("write", &json!({"path": "new.rs"}))).is_ok(),
            "main never claims"
        );
        let patch = json!({"patch": "*** Begin Patch\n*** Update File: f.rs\n@@\n-x\n+y\n*** End Patch"});
        assert!(b.check(&b.paths("apply_patch", &patch)).is_err());
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
