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
/// Reports longer than this reach the model cut, with an `agent read` pointer.
const REPORT_CUT: usize = 600;

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

#[derive(Debug, Clone, PartialEq)]
pub struct Def {
    pub name: String,
    pub description: String,
    /// Only these tools may run (the rest are refused, not hidden: siblings share a prefix).
    pub tools: Option<Vec<String>>,
    /// `provider/model`, or `small` for the provider's small model.
    pub model: Option<String>,
    pub thinking: Option<String>,
    pub prompt: String,
    /// The prompt replaces the preamble instead of being added after it.
    pub replace: bool,
}

fn builtins() -> Vec<Def> {
    let def = |name: &str, description: &str, tools: Option<&[&str]>, model: Option<&str>, prompt: &str| Def {
        name: name.into(),
        description: description.into(),
        tools: tools.map(|t| t.iter().map(|s| s.to_string()).collect()),
        model: model.map(str::to_string),
        thinking: None,
        prompt: prompt.into(),
        replace: false,
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
        let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|e| e == "md"))
            .collect();
        files.sort();
        for f in files {
            let Ok(text) = std::fs::read_to_string(&f) else {
                continue;
            };
            let (fm, body) = novacode_core::resources::frontmatter(&text);
            let name = fm
                .get("name")
                .cloned()
                .or_else(|| f.file_stem().map(|s| s.to_string_lossy().to_string()))
                .unwrap_or_default();
            if name.is_empty() || !seen.admit("agent", &name, &f) {
                continue;
            }
            let tools = fm.get("tools").map(|v| {
                v.split(',')
                    .map(|s| crate::import::nova_tool(s.trim()))
                    .filter(|s| !s.is_empty())
                    .collect()
            });
            // Claude Code's `inherit` and bare family names aren't routes: use the parent's model.
            let model = fm.get("model").filter(|m| m.contains('/') || *m == "small").cloned();
            all.insert(
                name.clone(),
                Def {
                    name,
                    description: fm.get("description").cloned().unwrap_or_default(),
                    tools,
                    model,
                    thinking: fm.get("thinking").cloned(),
                    prompt: body.trim().to_string(),
                    replace: fm
                        .get("mode")
                        .or(fm.get("systemPromptMode"))
                        .is_some_and(|m| m == "replace"),
                },
            );
        }
    }
    for d in builtins() {
        all.entry(d.name.clone()).or_insert(d);
    }
    (all.into_values().collect(), seen.shadowed)
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
    Path::new(&format!("/proc/{pid}")).exists()
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
    notify: Notify_,
    after: Vec<String>,
    status: Status,
    session: Option<String>,
    started: Option<Instant>,
    started_ms: u64,
    ended_ms: Option<u64>,
    last_activity: Instant,
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
}

pub struct Supervisor {
    children: Mutex<Vec<Child>>,
    changed: Notify,
    /// The parent session: its windows answer the children's questions.
    slot: HubSlot,
    place: Place,
    me: String,
    home: PathBuf,
    cwd: PathBuf,
    trusted: bool,
    exe: PathBuf,
    stall: Duration,
    timeout: Duration,
    defs: Vec<Def>,
    small_model: Option<String>,
    waiting: Mutex<usize>,
    /// Nerd Font glyphs in the panel rows (`ui.icons`).
    nerd: bool,
}

pub struct Setup {
    pub slot: HubSlot,
    pub me: String,
    pub home: PathBuf,
    pub cwd: PathBuf,
    pub trusted: bool,
    /// Agent-file folders, first wins (L7.12).
    pub agent_dirs: Vec<PathBuf>,
    pub stall_min: u64,
    pub timeout_min: u64,
    pub small_model: Option<String>,
    /// The child binary (tests use a fake); default: this one.
    pub exe: Option<PathBuf>,
    pub nerd: bool,
}

impl Supervisor {
    pub fn new(s: Setup) -> Arc<Self> {
        let sup = Arc::new(Supervisor {
            children: Mutex::default(),
            changed: Notify::new(),
            slot: s.slot,
            place: Place::from_env(),
            me: s.me,
            defs: defs(&s.agent_dirs).0,
            home: s.home,
            cwd: s.cwd,
            trusted: s.trusted,
            exe: s
                .exe
                .unwrap_or_else(|| std::env::current_exe().unwrap_or_else(|_| "novacode".into())),
            stall: Duration::from_secs(s.stall_min * 60),
            timeout: Duration::from_secs(s.timeout_min * 60),
            small_model: s.small_model,
            waiting: Mutex::new(0),
            nerd: s.nerd,
        });
        // Stall and timeout watchdog (L10.1: 10 min without activity, 45 min in all).
        let weak = Arc::downgrade(&sup);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(15)).await;
                let Some(s) = weak.upgrade() else { break };
                let mut stop = Vec::new();
                for c in s.children.lock().unwrap().iter() {
                    if matches!(c.status, Status::Running | Status::Starting) {
                        if c.last_activity.elapsed() > s.stall {
                            stop.push((c.n.clone(), "stalled: no activity"));
                        } else if c.started.is_some_and(|t| t.elapsed() > s.timeout) {
                            stop.push((c.n.clone(), "timed out"));
                        }
                    }
                }
                for (n, why) in stop {
                    s.stop(&n, why);
                }
            }
        });
        sup
    }

    fn handle(&self) -> Option<novacode_core::agent::SessionHandle> {
        self.slot.0.get().and_then(|w| w.upgrade()).map(|h| h.handle.clone())
    }

    fn parent_model(&self) -> Option<String> {
        self.slot.0.get().and_then(|w| w.upgrade()).map(|h| h.state().model)
    }

    /// Show a child in the Agent tab (L9.16 Subagents).
    fn publish(&self, c: &Child) {
        let Some(h) = self.handle() else { return };
        // Like the tmux swarm pane: type + id, then the title, then tokens and tool
        // calls; while it works, what it's doing.
        let (bot, db, wrench) = if self.nerd {
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
        let c = g
            .iter_mut()
            .find(|c| c.n == n || c.session.as_deref().is_some_and(|s| s.starts_with(n)))?;
        let r = f(c);
        self.publish(c);
        drop(g);
        self.changed.notify_waiters();
        Some(r)
    }

    /// Run the `auditor` agent on `task` and return its whole report (the goal tool).
    pub async fn audit(self: &Arc<Self>, task: String, title: String, model: Option<String>) -> Result<String, String> {
        let id = self.spawn(SpawnArgs {
            task,
            title: Some(title),
            agent: Some("auditor".into()),
            model,
            cwd: None,
            after: Vec::new(),
            notify: Notify_::Quiet,
            worktree: false,
        })?;
        self.wait(vec![id.clone()], false, self.timeout).await;
        let g = self.children.lock().unwrap();
        let c = g.iter().find(|c| c.n == id).ok_or("the auditor vanished")?;
        match (c.status, &c.report) {
            (Status::Done, Some(r)) => Ok(r.clone()),
            _ => Err(format!("the auditor didn't finish ({})", c.status.word())),
        }
    }

    fn spawn(self: &Arc<Self>, a: SpawnArgs) -> Result<String, String> {
        if self.place.depth >= MAX_DEPTH {
            return Err(format!("subagents can't spawn below depth {MAX_DEPTH}"));
        }
        let agent = a.agent.unwrap_or_else(|| "worker".into());
        let def = self.defs.iter().find(|d| d.name == agent).ok_or_else(|| {
            let names: Vec<&str> = self.defs.iter().map(|d| d.name.as_str()).collect();
            format!("no agent `{agent}`; known: {}", names.join(", "))
        })?;
        let parent = self.parent_model().unwrap_or_default();
        let mut model = match a.model.or(def.model.clone()).as_deref() {
            Some("small") => self.small_model.clone().unwrap_or(parent),
            Some(m) => m.to_string(),
            None => parent,
        };
        if let Some(t) = &def.thinking
            && !model.contains(':')
        {
            model = format!("{model}:{t}");
        }
        let mut cwd = match &a.cwd {
            Some(c) => at(&self.cwd, c),
            None => self.cwd.clone(),
        };
        let mut g = self.children.lock().unwrap();
        let n = format!("a{}", g.len() + 1);
        let title = a.title.unwrap_or_else(|| a.task.chars().take(40).collect());
        if a.worktree {
            let name = format!("{}-{n}", slug(&title));
            let (path, _) = novacode_tools::worktree::create(&cwd, &name)?;
            cwd = path;
        }
        for d in &a.after {
            if !g.iter().any(|c| &c.n == d) {
                return Err(format!("`after`: no agent {d}"));
            }
        }
        g.push(Child {
            n: n.clone(),
            title,
            agent,
            model,
            task: a.task,
            cwd,
            notify: a.notify,
            after: a.after,
            status: Status::Queued,
            session: None,
            started: None,
            started_ms: 0,
            ended_ms: None,
            last_activity: Instant::now(),
            tx: None,
            pid: None,
            last_text: String::new(),
            report: None,
            log: VecDeque::new(),
            tokens: 0,
            tool_calls: 0,
            delivered: false,
        });
        drop(g);
        self.start_ready();
        Ok(n)
    }

    /// Start every queued child whose `after` agents are all done.
    fn start_ready(self: &Arc<Self>) {
        // Pick and mark in one lock: parallel spawns must not start a child twice.
        let ready: Vec<(String, String)> = {
            let mut g = self.children.lock().unwrap();
            let done: Vec<String> = g
                .iter()
                .filter(|c| c.status == Status::Done)
                .map(|c| c.n.clone())
                .collect();
            g.iter_mut()
                .filter(|c| c.status == Status::Queued && c.after.iter().all(|d| done.contains(d)))
                .map(|c| {
                    c.status = Status::Starting;
                    let first = format!(
                        "You are subagent \"{}\" (agent {}, depth {}), started by another novacode agent. \
                         Do the task below. Your final message is your report to it: what you did, what you \
                         found with file paths, and anything left open.\n\n{}",
                        c.title,
                        c.agent,
                        self.place.depth + 1,
                        c.task
                    );
                    (c.n.clone(), first)
                })
                .collect()
        };
        for (n, first) in ready {
            let me = self.clone();
            tokio::spawn(async move { me.run(n, Some(first), None).await });
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
        let Some((model, agent, cwd, title)) = self.with(&n, |c| {
            c.status = Status::Starting;
            c.started = Some(Instant::now());
            c.started_ms = novacode_core::session::now_ms();
            c.ended_ms = None;
            c.report = None;
            c.delivered = false;
            c.last_activity = Instant::now();
            (c.model.clone(), c.agent.clone(), c.cwd.clone(), c.title.clone())
        }) else {
            return;
        };
        let mut cmd = tokio::process::Command::new(&self.exe);
        cmd.arg("--rpc").args(["-m", &model]).args(["--agent", &agent]);
        if self.trusted {
            cmd.arg("--trust");
        }
        if let Some(s) = &session {
            cmd.args(["-r", s]);
        }
        let mut chain = self.place.chain.clone();
        chain.push(self.me.clone());
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
            AgentEvent::AgentStart => {
                *prompted = true;
                self.with(n, |c| {
                    c.status = Status::Running;
                    c.last_activity = Instant::now();
                });
            }
            AgentEvent::ToolStart { call } => {
                let (name, args) = (call.name, call.arguments);
                let what = args
                    .get("path")
                    .or(args.get("command"))
                    .or(args.get("pattern"))
                    .and_then(Value::as_str)
                    .map(|s| s.chars().take(60).collect::<String>())
                    .unwrap_or_default();
                self.with(n, |c| {
                    c.last_activity = Instant::now();
                    c.tool_calls += 1;
                    c.log.push_back(format!("{name} {what}").trim().to_string());
                    if c.log.len() > 200 {
                        c.log.pop_front();
                    }
                });
            }
            AgentEvent::Stream(StreamEvent::Delta { .. }) => {
                // Streaming counts as activity; no publish per delta.
                if let Some(c) = self.children.lock().unwrap().iter_mut().find(|c| c.n == n) {
                    c.last_activity = Instant::now();
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
        let (me, n) = (self.clone(), n.to_string());
        tokio::spawn(async move {
            let replies = me.slot.ask(questions).await;
            me.send_line(&n, json!({"type": "answer", "ask": id, "replies": replies}));
            me.with(&n, |c| c.status = Status::Running);
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
        let (me, n) = (self.clone(), n.to_string());
        tokio::spawn(async move {
            let answer = match me.slot.approve(req).await {
                Approval::Once => json!("once"),
                Approval::Session => json!("session"),
                Approval::Always => json!("always"),
                Approval::Deny(why) => json!({"deny": why}),
            };
            me.send_line(&n, json!({"type": "approval", "approval": id, "answer": answer}));
            me.with(&n, |c| c.status = Status::Running);
        });
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
            (
                c.title.clone(),
                c.report.clone().unwrap_or_default(),
                c.notify,
                c.started.map_or(0, |t| t.elapsed().as_secs()),
            )
        }) else {
            return;
        };
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
        let session = session.ok_or(format!("{n} never started; nothing to resume"))?;
        self.with(n, |c| {
            c.status = Status::Starting;
            c.delivered = false;
        });
        let (me, n2) = (self.clone(), n.to_string());
        tokio::spawn(async move { me.run(n2, Some(text), Some(session)).await });
        Ok(format!("resumed {n} with the message"))
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
        Some(rest) => std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_default()
            .join(rest),
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
}

pub struct AgentTool {
    pub sup: Arc<Supervisor>,
    description: String,
}

impl AgentTool {
    pub fn new(sup: Arc<Supervisor>) -> Self {
        let agents: Vec<String> = sup
            .defs
            .iter()
            .map(|d| format!("- {}: {}", d.name, d.description))
            .collect();
        AgentTool {
            description: format!(
                "Run subagents: separate novacode sessions that work in the background and report back.\n\
                 Actions: spawn {{task, title?, agent?, model?, cwd?, after?: [ids], notify?: wake|next|quiet, worktree?}} → id; \
                 list; status {{id}}; log {{id, limit?}}; read {{id, offset?, limit?}} (full report); \
                 send {{id, message}} (steers a running agent, resumes a finished one); \
                 wait {{ids?, any?, timeout?}} (blocks until reports arrive, default all, 600 s); stop {{id}}.\n\
                 Reports arrive by themselves (notify wake starts a turn); don't poll. \
                 Two agents never edit the same file: the second is refused.\nAgents:\n{}",
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
                        Ok(n) => ToolOutput {
                            details: Some(serde_json::json!({ "id": n })),
                            ..ToolOutput::text(format!("started {n}; its report arrives when it's done"))
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
            format!("print({hello:?}.replace('SESSION', str(uuid.uuid4())), flush=True)"),
            "for line in sys.stdin:".into(),
            "    m = json.loads(line)".into(),
            "    if m['type'] != 'prompt': continue".into(),
            "    time.sleep(0.3)".into(),
            format!("    print({:?}, flush=True)", ev(AgentEvent::AgentStart)),
            format!(
                "    print({reply:?}.replace('REPLY', os.getcwd() + ' :: ' + m['text'].split(chr(10))[-1]), flush=True)"
            ),
            format!("    print({:?}, flush=True)", ev(AgentEvent::Settled)),
        ];
        std::fs::write(&script, lines.join("\n")).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        script
    }

    #[tokio::test]
    async fn spawn_after_wait_and_send() {
        let tmp = std::env::temp_dir().join(format!("nc-sup-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        let sup = Supervisor::new(Setup {
            slot: HubSlot::default(),
            me: "ROOT".into(),
            home: tmp.clone(),
            cwd: tmp.clone(),
            trusted: false,
            agent_dirs: vec![],
            stall_min: 10,
            timeout_min: 45,
            small_model: None,
            exe: Some(fake_child(&tmp)),
            nerd: false,
        });
        let spawn = |task: &str, after: &[&str]| {
            sup.spawn(serde_json::from_value(json!({"task": task, "after": after, "model": "p/m"})).unwrap())
                .unwrap()
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
        let _ = std::fs::remove_dir_all(&tmp);
    }

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
            "---\nname: rev\ndescription: reviews\ntools: Read, Grep, Glob\nmodel: sonnet\n---\nReview.",
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
        assert_eq!(rev.model, None, "`sonnet` isn't a route: use the parent's model");
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
