//! The agent loop (L3.1) and the session owner task (L8.1, L0.2).
//!
//! One state machine: take queued input → call the model (retrying per L4.6)
//! → run tools → decide whether to continue. It never fails: errors end the
//! run as an `AgentEnd` with the stop reason, and every effect is an appended
//! session entry plus an event.

use crate::pipeline::{self, Prepared};
use crate::policy::{Approver, Permissions, Policy};
use crate::recovery::{Nudge, Recovery};
use crate::session::{EntryKind, Store};
use novacode_ai::adapter::{Adapter, CacheRetention, Request};
use novacode_ai::retry;
use novacode_ai::schema::{self, Quirks};
use novacode_base::redact::Redactor;
use novacode_tools::guard::Budget;
use novacode_tools::{ExecutionEnv, FileState, Tool, ToolCtx, ToolOutput};
use novacode_types::{
    AgentEvent, AssistantMessage, Block, ErrorKind, Message, StopReason, StreamEvent, ThinkingLevel, ToolCall, ToolDef,
    Usage,
};
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::{Notify, broadcast, mpsc};
use tokio_util::sync::CancellationToken;

/// Bound on buffered events per subscriber; a lagging one misses events, never blocks the loop.
pub const EVENT_BUFFER: usize = 4096;

#[derive(Debug, Clone)]
pub struct Config {
    pub cwd: PathBuf,
    pub provider: String,
    pub model: String,
    pub max_tokens: u32,
    pub cache: CacheRetention,
    pub window_tokens: u64,
    pub thinking: ThinkingLevel,
    /// The model takes images (L2.15).
    pub images: bool,
    /// The model reasons: only then is a thinking level sent (L2.16).
    pub reasoning: bool,
    /// Where cut tool output is spilled in full.
    pub spill_dir: PathBuf,
    /// Masks novacode's own secrets in tool output (L6.6).
    pub redactor: Redactor,
    /// Where the system prompt's parts come from (L4.7).
    pub prompt: crate::prompt::Sources,
    /// Keep the prompt cache warm while it pays off (L2.17, setting `cache.warm`, off by default).
    pub warm: bool,
    /// Record dev metrics to `<home>/metrics/<session>.jsonl` (L11.11, setting `debug.metrics`).
    pub metrics: bool,
}

/// A model change, possibly to another provider (L2.11): everything that
/// depends on the model travels together.
pub struct Switch {
    pub provider: String,
    pub model: String,
    pub adapter: Arc<dyn Adapter>,
    pub window_tokens: u64,
    pub max_tokens: u32,
    pub images: bool,
    pub reasoning: bool,
    /// Already clamped to what the new model supports.
    pub thinking: ThinkingLevel,
    /// The observer's model after the switch (the new provider's small model);
    /// `None` keeps the current one (a `memory.model` setting).
    pub helper: Option<crate::memory::Worker>,
}

/// Live state `/reload` hands to the rebuilt session: everything that isn't config.
pub struct Carry {
    /// Where the session is now (a worktree move included).
    pub cwd: PathBuf,
    /// Files read this session: edits keep working without a re-read.
    pub files: Arc<FileState>,
    /// Background processes keep running.
    pub processes: Option<Arc<novacode_tools::process::ProcessManager>>,
    /// "Allow for this session" answers.
    pub allow: Vec<crate::policy::Rule>,
    /// The mode preset that is on (plan mode, …).
    pub mode: Option<String>,
    /// `SessionStart` already ran: its context isn't added again.
    pub started: bool,
}

pub struct Agent {
    adapter: Arc<dyn Adapter>,
    tools: Vec<Arc<dyn Tool>>,
    /// Fixed at session start (L4.8): the tool list never changes mid-session.
    defs: Vec<ToolDef>,
    pub store: Store,
    cfg: Config,
    env: Arc<dyn ExecutionEnv>,
    files: Arc<FileState>,
    events: broadcast::Sender<AgentEvent>,
    shared: Arc<Shared>,
    policy: Arc<Policy>,
    modes: Option<Arc<crate::modes::Modes>>,
    /// `None` = nobody to ask (print mode, headless): asks become refusals or justifications.
    approver: Option<Arc<dyn Approver>>,
    /// The command judge (L6.8); `None` = off.
    judge: Option<Arc<crate::judge::Judge>>,
    /// First loop retry wait; later ones double (L4.6).
    retry_base: Duration,
    /// Learned tool-schema rejections (L2.20).
    quirks: Arc<Quirks>,
    /// Observer and compaction (L4.5); `None` = compaction off.
    memory: Option<Memory>,
    /// A compaction happened since the last request: its miss is deliberate.
    compacted: bool,
    /// The "session not saved" notice was shown; cleared once it saves again (L4.16).
    save_error: bool,
    /// Kept replies carry usage from before a compaction: estimate the context instead.
    usage_stale: bool,
    /// Where file checkpoints go (L4.15); `None` = off.
    checkpoints: Option<crate::checkpoint::Repo>,
    /// The run's snapshot, taken while the model thinks; awaited before any tool runs.
    pending_cp: Option<tokio::task::JoinHandle<crate::checkpoint::Snapshot>>,
    /// Long-running commands (L5.11); dropping the agent kills them.
    processes: Option<Arc<novacode_tools::process::ProcessManager>>,
    /// Found through `tool_search`, never in the request (L5.17).
    deferred: Vec<Arc<dyn Tool>>,
    /// Codemode-only tools (L5.16).
    scripted: Vec<Arc<dyn Tool>>,
    /// MCP servers (L7.4); scripts see tools of servers that connected after start too.
    mcp: Option<Arc<crate::mcp::Hub>>,
    /// before/after tool handlers (L7.1, L7.3).
    hooks: Arc<crate::hooks::Hooks>,
    /// Script extensions (L7.2): commands for the host.
    extensions: Option<Arc<crate::extensions::Extensions>>,
    /// Claude Code plugin hooks (L7.14) and the state they need.
    plugins: Option<Plugins>,
}

struct Plugins {
    hooks: Arc<crate::plugin_hooks::PluginHooks>,
    /// `SessionStart` has run (it runs before the first model call).
    started: bool,
    /// The last run was started by a `Stop` hook's block (`stop_hook_active`).
    stop_active: bool,
}

/// Where a model's tool call goes (L5.17).
enum Resolved {
    Listed(ToolCall),
    /// Through `call`, or announced in-line.
    Deferred(ToolCall),
    /// Deferred and not found yet.
    Locked(ToolCall),
}

struct Memory {
    /// Where reflections are promoted across sessions (L10.4).
    store: Option<crate::memstore::Memories>,
    worker: crate::memory::Worker,
    settings: crate::memory::Settings,
    /// The background observer run, if one is going.
    task: Option<tokio::task::JoinHandle<ObserverRun>>,
    /// `memory.enabled: false`: no observer; compaction summarises with the session model.
    observe: bool,
}

/// What a background observer run produced.
struct ObserverRun {
    covers: String,
    /// Entries to store (notes, then reflections and drops), even when a later step failed.
    entries: Vec<EntryKind>,
    error: Option<String>,
}

#[derive(Default)]
struct Queues {
    /// User content: text, plus any attached images (L2.22).
    steering: VecDeque<Vec<Block>>,
    follow_up: VecDeque<Vec<Block>>,
    /// Notes for the model (process exits, log matches): stored as reminders at
    /// the next model call (L4.12).
    reminders: VecDeque<(String, String)>,
    /// A reminder that should start a run when idle.
    wake: bool,
}

#[derive(Default)]
struct Shared {
    queues: Mutex<Queues>,
    /// The running turn's token; a stale abort can't hit the next run (L3.6).
    cancel: Mutex<Option<CancellationToken>>,
    notify: Notify,
    closed: Mutex<bool>,
    /// Route to switch to at the start of the next run.
    next_model: Mutex<Option<Switch>>,
    next_thinking: Mutex<Option<ThinkingLevel>>,
    /// Tools a client registered (L7.8), added at the start of the next run.
    new_tools: Mutex<Vec<Arc<dyn Tool>>>,
    /// What the context holds, refreshed after each run (for `/context`).
    report: Mutex<ContextReport>,
    /// The last request that touched the cache, for the miss detector and the warmer (L2.17).
    sent: Mutex<Option<crate::cache::Sent>>,
    /// The reply to `sent` and the `/btw` side thread on top of them.
    side: Mutex<Side>,
    warm: Mutex<Warm>,
    warm_wake: Notify,
    /// `/compact` was asked for; the owner compacts when idle.
    compact: Mutex<bool>,
    /// `/name`: written between runs (L4.4).
    rename: Mutex<Option<String>>,
    /// `/worktree`: the folder to move the session to, between runs.
    move_to: Mutex<Option<PathBuf>>,
    /// The session's prompts as a tree, refreshed with the report (for `/tree`).
    prompts: Mutex<Vec<crate::session::PromptNode>>,
    /// What extensions read and call from outside the owner task (L7.11).
    ext: Mutex<ExtView>,
    /// `novacode.state` entries: what the branch holds plus what extensions added
    /// since (`pending`, written to the session by the owner task).
    ext_state: Mutex<ExtState>,
    /// Plugin hooks, for skills the user activates from outside the owner task (L7.16).
    plugin_hooks: std::sync::OnceLock<Arc<crate::plugin_hooks::PluginHooks>>,
}

#[derive(Default)]
struct ExtState {
    /// (extension, kind, data) on the current branch, oldest first.
    live: Vec<(String, String, serde_json::Value)>,
    pending: Vec<(String, String, serde_json::Value)>,
}

/// Refreshed with the report: extension handlers run while the owner is busy, so
/// they read this copy instead of asking the owner (which could deadlock).
#[derive(Default)]
struct ExtView {
    /// The conversation as of the last refresh, without the system prompt.
    history: Arc<Vec<Message>>,
    /// The session's adapter and a request template (model, limits, session id).
    route: Option<(Arc<dyn Adapter>, Request)>,
    /// The pipeline runner and a context for extension tool calls (`spawnAgent`).
    tools: Option<(Arc<crate::codemode::Runner>, ToolCtx)>,
    /// The codemode tool, for `/btw` scripts limited to read-only tools.
    codemode: Option<Arc<dyn Tool>>,
}

/// What `/btw` may run: tools that can't change anything.
const SIDE_TOOLS: [&str; 4] = ["read", "grep", "find", "ls"];
/// Tool rounds per side question before it must answer.
const SIDE_ROUNDS: usize = 5;

/// A runner with `r`'s rules and hooks but only `tools`, and no approver:
/// anything that would need an approval is refused instead of prompting over the popup.
fn side_runner(r: &crate::codemode::Runner, tools: Vec<Arc<dyn Tool>>) -> Arc<crate::codemode::Runner> {
    Arc::new(crate::codemode::Runner {
        tools,
        policy: r.policy.clone(),
        approver: None,
        spill_dir: r.spill_dir.clone(),
        redactor: r.redactor.clone(),
        budget: r.budget,
        seq: Default::default(),
        hooks: r.hooks.clone(),
        judge: r.judge.clone(),
        // Its own clock: a side question's waits are not the main script's.
        wait: Default::default(),
    })
}

impl Shared {
    fn remind(&self, kind: &str, text: String, wake: bool) {
        let mut q = self.queues.lock().unwrap();
        q.reminders.push_back((kind.to_string(), text));
        q.wake |= wake;
        drop(q);
        self.notify.notify_one();
    }
}

/// Warmer state, reset by every real request.
#[derive(Default)]
struct Side {
    reply: Option<AssistantMessage>,
    /// (question, answer) pairs, valid while the main thread hasn't moved on.
    thread: Vec<(String, String)>,
}

#[derive(Default)]
struct Warm {
    adapter: Option<Arc<dyn Adapter>>,
    /// Bumped by each real request: a warm decision made before it is stale.
    generation: u64,
    started: Option<Instant>,
    busy: bool,
    idle_warms: u32,
    stopped: bool,
    status: String,
    /// Replays done since the owner last stored them: (provider, model, usage).
    usage: Vec<(String, String, Usage)>,
}

/// Where the context window goes, in estimated tokens (L4.7 `/context`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ContextReport {
    pub sections: Vec<(String, u64)>,
    pub tools: Vec<(String, u64)>,
    /// Everything after the prompt: messages, deltas, reminders.
    pub messages: u64,
    pub window: u64,
    /// Cache warmer state, one line.
    pub warm: String,
}

impl ContextReport {
    pub fn render(&self) -> String {
        use std::fmt::Write;
        let sum = |v: &[(String, u64)]| v.iter().map(|(_, t)| t).sum::<u64>();
        let total = sum(&self.sections) + sum(&self.tools) + self.messages;
        let pct = |t: u64| t as f64 * 100.0 / self.window.max(1) as f64;
        let mut s = String::new();
        let mut line = |name: &str, t: u64| {
            let _ = writeln!(s, "  {name:<24} {t:>8}  {:>5.1}%", pct(t));
        };
        for (title, rows) in [("system prompt", &self.sections), ("tools", &self.tools)] {
            line(title, sum(rows));
            for (n, t) in rows {
                line(&format!("  {n}"), *t);
            }
        }
        line("messages", self.messages);
        let _ = write!(
            s,
            "\n  {:<24} {total:>8}  {:>5.1}% of {}k (estimated)",
            "total",
            pct(total),
            self.window / 1000
        );
        if !self.warm.is_empty() {
            let _ = write!(s, "\n\n  cache warmer: {}", self.warm);
        }
        s
    }
}

impl Agent {
    pub fn new(
        adapter: Arc<dyn Adapter>,
        tools: Vec<Arc<dyn Tool>>,
        mut store: Store,
        mut cfg: Config,
        env: Arc<dyn ExecutionEnv>,
    ) -> Self {
        cfg.prompt.tool_guidelines = novacode_tools::guidelines(&tools);
        refresh_prompt(&mut store, &cfg.prompt);
        if store.last_model() != Some((cfg.provider.clone(), cfg.model.clone())) {
            log_err(store.append(EntryKind::ModelChange {
                provider: cfg.provider.clone(),
                model: cfg.model.clone(),
            }));
        }
        if store.last_thinking() != Some(cfg.thinking) {
            log_err(store.append(EntryKind::ThinkingChange { level: cfg.thinking }));
        }
        let defs: Vec<ToolDef> = tools.iter().map(|t| pipeline::def(t.as_ref())).collect();
        let (events, _) = broadcast::channel(EVENT_BUFFER);
        let home = novacode_base::platform::home().unwrap_or_default();
        // Compacted after the last reply (e.g. at the end of a `-p` run): the next miss is expected.
        let compacted = {
            let b = store.branch();
            let last = |f: &dyn Fn(&EntryKind) -> bool| b.iter().rposition(|e| f(&e.kind));
            last(&|k| matches!(k, EntryKind::Compaction { .. }))
                > last(&|k| {
                    matches!(
                        k,
                        EntryKind::Message {
                            message: Message::Assistant(_)
                        }
                    )
                })
        };
        let shared: Arc<Shared> = Arc::default();
        // A resumed session: its last request, so the first reply is checked for a miss too.
        if let Some((messages, provider, model, usage, ts)) = store.last_request() {
            let ago = Duration::from_millis(crate::session::now_ms().saturating_sub(ts));
            *shared.sent.lock().unwrap() = Some(crate::cache::Sent {
                req: Request {
                    tail: Vec::new(),
                    model,
                    messages,
                    tools: defs.clone(),
                    max_tokens: cfg.max_tokens,
                    cache: cfg.cache,
                    thinking: cfg.reasoning.then_some(cfg.thinking),
                    images: cfg.images,
                    session: Some(store.id().to_string()),
                },
                provider,
                usage,
                at: Instant::now().checked_sub(ago).unwrap_or_else(Instant::now),
            });
        }
        let agent = Self {
            adapter,
            tools,
            defs,
            store,
            cfg,
            env,
            files: Arc::new(FileState::default()),
            events,
            shared,
            policy: Arc::new(Policy::new(Permissions::default(), home, None)),
            modes: None,
            approver: None,
            judge: None,
            retry_base: LOOP_RETRY_BASE,
            quirks: Arc::new(Quirks::ephemeral()),
            memory: None,
            compacted,
            save_error: false,
            usage_stale: compacted,
            checkpoints: None,
            processes: None,
            deferred: Vec::new(),
            scripted: Vec::new(),
            mcp: None,
            hooks: Arc::default(),
            extensions: None,
            plugins: None,
            pending_cp: None,
        };
        agent.update_report();
        agent
    }

    fn update_report(&self) {
        use novacode_types::estimate;
        *self.shared.prompts.lock().unwrap() = self.store.prompts();
        let sections: Vec<(String, u64)> = self
            .store
            .sections()
            .into_iter()
            .map(|(n, t)| (n, estimate::text(&t)))
            .collect();
        let tools = self
            .defs
            .iter()
            .map(|d| {
                let json = serde_json::to_string(&d.schema).unwrap_or_default();
                (d.name.clone(), estimate::text(&d.description) + estimate::text(&json))
            })
            .collect();
        let projection = self.store.projection();
        // The leading system messages are the prompt, counted per section above.
        let lead = projection
            .iter()
            .take_while(|m| matches!(m, Message::System { .. }))
            .count();
        let messages = projection[lead..].iter().map(estimate::message).sum();
        // Always: `/btw` runs its tools through this view; the history copy only for extensions.
        let history = match self.extensions {
            Some(_) => projection[lead..].to_vec(),
            None => Vec::new(),
        };
        self.refresh_ext(history);
        let warm = if self.cfg.warm {
            self.shared.warm.lock().unwrap().status.clone()
        } else {
            "off (setting cache.warm)".into()
        };
        *self.shared.report.lock().unwrap() = ContextReport {
            sections,
            tools,
            messages,
            window: self.cfg.window_tokens,
            warm,
        };
    }

    /// The pipeline for nested calls (codemode, extensions): every tool but codemode,
    /// with the same rules, approvals and hooks as the model's own calls (L3.11).
    fn runner(&self, budget: Budget) -> Arc<crate::codemode::Runner> {
        let mut tools: Vec<Arc<dyn Tool>> = self
            .tools
            .iter()
            .chain(&self.deferred)
            .chain(&self.scripted)
            .filter(|t| t.name() != crate::codemode::NAME)
            .cloned()
            .collect();
        if let Some(hub) = &self.mcp {
            for t in hub.tools() {
                if t.exposure() != novacode_tools::Exposure::Hidden && !tools.iter().any(|x| x.name() == t.name()) {
                    tools.push(t);
                }
            }
        }
        Arc::new(crate::codemode::Runner {
            tools,
            policy: self.policy.clone(),
            approver: self.approver.clone(),
            spill_dir: self.cfg.spill_dir.clone(),
            redactor: self.cfg.redactor.clone(),
            budget,
            seq: Default::default(),
            hooks: self.hooks.clone(),
            judge: self.judge.clone(),
            wait: Default::default(),
        })
    }

    /// New copy of what extensions see (`session.messages()`, `complete()`, `spawnAgent()`).
    fn refresh_ext(&self, history: Vec<Message>) {
        let budget = Budget {
            window_tokens: self.cfg.window_tokens,
            used_tokens: novacode_types::estimate::context(&history),
        };
        let ctx = ToolCtx {
            cwd: self.cfg.cwd.clone(),
            env: self.env.clone(),
            cancel: CancellationToken::new(),
            updates: None,
            files: self.files.clone(),
            session_id: self.store.id().to_string(),
            processes: self.processes.clone(),
            native_tools: false,
            nested: None,
            script: false,
        };
        let template = Request {
            tail: Vec::new(),
            model: self.cfg.model.clone(),
            messages: Vec::new(),
            tools: Vec::new(),
            max_tokens: self.cfg.max_tokens,
            cache: CacheRetention::Short,
            thinking: None,
            images: self.cfg.images,
            session: Some(self.store.id().to_string()),
        };
        {
            let mut st = self.shared.ext_state.lock().unwrap();
            let mut live: Vec<_> = self
                .store
                .branch()
                .into_iter()
                .filter_map(|e| match &e.kind {
                    EntryKind::ExtState { ext, kind, data } => Some((ext.clone(), kind.clone(), data.clone())),
                    _ => None,
                })
                .collect();
            live.extend(st.pending.iter().cloned());
            st.live = live;
        }
        *self.shared.ext.lock().unwrap() = ExtView {
            history: Arc::new(history),
            route: Some((self.adapter.clone(), template)),
            tools: Some((self.runner(budget), ctx)),
            codemode: self.tools.iter().find(|t| t.name() == crate::codemode::NAME).cloned(),
        };
    }

    /// Miss detection and warmer reset after a real request (L2.17).
    fn after_request(&mut self, req: Request, usage: &Usage) {
        let ttl = self.adapter.warm_ttl(&req);
        let prev = self.shared.sent.lock().unwrap().take();
        let compacted = std::mem::take(&mut self.compacted);
        if let Some(mut m) = prev.and_then(|p| crate::cache::miss(&p, &self.cfg.provider, &req, usage, ttl)) {
            if compacted {
                m.cause = "compaction (expected once)".into();
            }
            let text = format!("cache miss: {} prompt tokens not read from cache ({})", m.lost, m.cause);
            self.emit(AgentEvent::Stream(StreamEvent::Notice { text }));
            let line = serde_json::json!({
                "session": self.store.id(),
                "provider": self.cfg.provider,
                "model": self.cfg.model,
                "lost": m.lost,
                "cause": m.cause,
                "usage": usage,
            });
            let path = self.cfg.prompt.home.join("logs").join("cache-misses.jsonl");
            let _ = std::fs::create_dir_all(path.parent().unwrap_or(&path));
            if let Err(e) = novacode_base::storage::append_jsonl(&path, &line) {
                tracing::warn!(%e, "cache miss log");
            }
        }
        *self.shared.sent.lock().unwrap() = Some(crate::cache::Sent {
            req,
            provider: self.cfg.provider.clone(),
            usage: *usage,
            at: Instant::now(),
        });
        let mut w = self.shared.warm.lock().unwrap();
        w.generation += 1;
        w.started = Some(Instant::now());
        w.idle_warms = 0;
        w.stopped = ttl.is_none();
        w.adapter = Some(self.adapter.clone());
        w.status = if ttl.is_some() {
            "scheduled".into()
        } else {
            "this provider has no cache to keep warm".into()
        };
        drop(w);
        self.shared.warm_wake.notify_one();
    }

    fn set_busy(&self, busy: bool) {
        self.shared.warm.lock().unwrap().busy = busy;
        self.shared.warm_wake.notify_one();
    }

    /// Persist the usage of warm replays done since the last look.
    fn store_warm_usage(&mut self) {
        let done = std::mem::take(&mut self.shared.warm.lock().unwrap().usage);
        for (provider, model, usage) in done {
            self.add(EntryKind::WarmUsage { provider, model, usage });
        }
    }

    /// Take a file checkpoint at the start of every run (L4.15).
    /// Tools found through `tool_search` and run directly (once announced) or via `call`.
    pub fn with_deferred(mut self, tools: Vec<Arc<dyn Tool>>) -> Self {
        self.deferred = tools;
        self
    }

    /// Deferred tools announced in-line on this branch (L4.8).
    fn activated(&self) -> Vec<String> {
        let mut v = Vec::new();
        for m in self.store.projection() {
            if let Message::Custom { kind, data } = &m
                && kind == "tools_added"
            {
                v.extend(
                    data["tools"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(|t| t["name"].as_str().map(String::from)),
                );
            }
        }
        v
    }

    /// `call{name, arguments}` → the real call (same id), so the pipeline and
    /// permissions see the actual tool. Deferred tools run only via `call` or once announced.
    fn resolve_call(&self, c: ToolCall, activated: &[String]) -> Resolved {
        if c.name == "call" && !self.deferred.is_empty() {
            let name = c.arguments["name"].as_str().unwrap_or_default().to_string();
            let arguments = c
                .arguments
                .get("arguments")
                .cloned()
                .unwrap_or_else(|| serde_json::json!({}));
            return Resolved::Deferred(ToolCall {
                name,
                arguments,
                arguments_error: None,
                ..c
            });
        }
        match self.deferred.iter().any(|t| t.name() == c.name) {
            false => Resolved::Listed(c),
            true if activated.contains(&c.name) => Resolved::Deferred(c),
            true => Resolved::Locked(c),
        }
    }

    /// Give bash and the `process` tool a process manager (L5.11).
    pub fn with_processes(mut self, pm: Arc<novacode_tools::process::ProcessManager>) -> Self {
        self.processes = Some(pm);
        self
    }

    pub fn processes(&self) -> Option<&Arc<novacode_tools::process::ProcessManager>> {
        self.processes.as_ref()
    }

    pub fn with_checkpoints(mut self, repo: crate::checkpoint::Repo) -> Self {
        self.checkpoints = Some(repo);
        self
    }

    /// Paths with a pre-image on this branch (shadow stores).
    fn tracked_paths(&self) -> Vec<String> {
        let mut v: Vec<String> = Vec::new();
        for e in self.store.branch() {
            if let EntryKind::FileBefore { path, .. } = &e.kind
                && !v.contains(path)
            {
                v.push(path.clone());
            }
        }
        v
    }

    fn start_checkpoint(&mut self) {
        if let Some(repo) = self.checkpoints.clone() {
            let tracked = self.tracked_paths();
            self.pending_cp = Some(tokio::task::spawn_blocking(move || {
                crate::checkpoint::snapshot(&repo, &tracked)
            }));
        }
    }

    /// Store the run's checkpoint once it's taken; tools wait for this.
    async fn finish_checkpoint(&mut self) {
        let Some(task) = self.pending_cp.take() else { return };
        match task.await {
            Ok(Ok((tree, skipped))) => {
                self.add(EntryKind::Checkpoint { tree, skipped });
            }
            Ok(Err(e)) => {
                tracing::warn!(error = %e, "checkpoint");
                self.emit(AgentEvent::Stream(StreamEvent::Notice {
                    text: format!("file checkpoint failed, this prompt can't rewind files: {e}"),
                }));
            }
            Err(_) => {}
        }
    }

    /// Outside git: keep a file's content before novacode first changes it.
    fn pre_images(&mut self, prepared: &[(ToolCall, Prepared)]) {
        let Some(repo) = self.checkpoints.clone().filter(|r| r.shadow.is_some()) else {
            return;
        };
        let mut tracked = self.tracked_paths();
        for (_, p) in prepared {
            let Prepared::Ready(t, args, _) = p else { continue };
            if !matches!(t.name(), "edit" | "write") {
                continue;
            }
            let Some(raw) = args["path"].as_str() else { continue };
            let abs = self.cfg.cwd.join(raw);
            let Ok(rel) = abs.strip_prefix(&repo.root) else {
                continue;
            };
            let rel = rel.to_string_lossy().to_string();
            if tracked.contains(&rel) {
                continue;
            }
            match crate::checkpoint::pre_image(&repo, &rel) {
                Ok(blob) => {
                    tracked.push(rel.clone());
                    self.add(EntryKind::FileBefore { path: rel, blob });
                }
                Err(e) => tracing::warn!(error = %e, "pre-image"),
            }
        }
    }

    /// Turn on the observer and compaction (L4.5).
    pub fn with_memory(mut self, worker: crate::memory::Worker, settings: crate::memory::Settings) -> Self {
        self.memory = Some(Memory {
            store: None,
            worker,
            settings,
            task: None,
            observe: true,
        });
        self
    }

    /// Compaction without the observer (`memory.enabled: false`, L4.5 revision).
    pub fn without_observer(mut self) -> Self {
        if let Some(m) = self.memory.as_mut() {
            m.observe = false;
        }
        self
    }

    /// Promote the observer's reflections into the project memory (L10.4).
    pub fn with_memory_store(mut self, store: crate::memstore::Memories) -> Self {
        if let Some(m) = self.memory.as_mut() {
            m.store = Some(store);
        }
        self
    }

    fn context_tokens(&self) -> u64 {
        let p = self.store.projection();
        if self.usage_stale {
            return p.iter().map(novacode_types::estimate::message).sum();
        }
        novacode_types::estimate::context(&p)
    }

    /// Store a finished observer run; with `wait`, wait for a running one.
    /// Write the `novacode.state` entries extensions added since the last flush.
    fn flush_ext_state(&mut self) {
        let pending = std::mem::take(&mut self.shared.ext_state.lock().unwrap().pending);
        for (ext, kind, data) in pending {
            self.add(EntryKind::ExtState { ext, kind, data });
        }
    }

    async fn collect_notes(&mut self, wait: bool) {
        let Some(m) = self.memory.as_mut() else { return };
        let Some(task) = m.task.take_if(|t| wait || t.is_finished()) else {
            return;
        };
        let Ok(run) = task.await else { return };
        // A branch switch while it ran: the notes belong to another branch.
        if self.store.branch().iter().any(|e| e.id == run.covers) {
            if let Some(mem) = m.store.clone() {
                let new: Vec<String> = run
                    .entries
                    .iter()
                    .filter_map(|e| match e {
                        EntryKind::Reflections { reflections, .. } => {
                            Some(reflections.iter().map(|r| r.content.clone()))
                        }
                        _ => None,
                    })
                    .flatten()
                    .collect();
                let session = self.store.id().to_string();
                if !new.is_empty() {
                    tokio::task::spawn_blocking(move || {
                        for text in new {
                            if let Err(e) =
                                mem.remember(crate::memstore::Scope::Project, &text, "reflection", false, &session)
                            {
                                tracing::warn!(%e, "memory promote");
                            }
                        }
                    });
                }
            }
            for e in run.entries {
                self.add(e);
            }
        }
        if let Some(e) = run.error {
            tracing::warn!(error = %e, "memory observer");
            self.emit(AgentEvent::Stream(StreamEvent::Notice {
                text: format!("memory observer failed: {e}"),
            }));
        }
    }

    /// After a run settles: store notes, compact when the context is past the
    /// threshold, else start the observer once enough new conversation built up.
    async fn after_settle(&mut self) {
        self.collect_notes(false).await;
        let Some(m) = self.memory.as_ref() else { return };
        if self.context_tokens() >= m.settings.compact_at {
            self.compact("the context reached the compaction threshold").await;
            return;
        }
        self.start_observer();
    }

    /// Between model calls of one long run: the same upkeep as after a run, so a
    /// run of hundreds of tool calls still gets notes and compacts at `compact_at`.
    async fn mid_run_memory(&mut self) {
        self.collect_notes(false).await;
        let Some(m) = self.memory.as_ref() else { return };
        let window = self.cfg.window_tokens;
        // `compact_at` at or past the window means auto-compaction is off.
        let at = if m.settings.compact_at < window {
            m.settings.compact_at.min(window * 95 / 100)
        } else {
            u64::MAX
        };
        if self.context_tokens() >= at {
            self.compact("the context reached the compaction threshold").await;
        } else {
            self.start_observer();
        }
    }

    /// Start the observer once enough new conversation built up since its last notes.
    fn start_observer(&mut self) {
        let Some(m) = self.memory.as_ref() else { return };
        if m.task.is_some() || !m.observe {
            return;
        }
        let path = self.store.branch();
        let from = crate::memory::watermark(&path);
        if crate::memory::tokens(&path, from, path.len()) < m.settings.observe_after {
            return;
        }
        let chunk = crate::memory::chunk(&path, from, path.len(), m.settings.chunk_tokens);
        if chunk.aliases.is_empty() {
            return;
        }
        let known = crate::memory::notes(&path);
        let active = crate::memory::active_notes(&path);
        let refl = crate::memory::reflections(&path);
        let end = path
            .iter()
            .position(|e| e.id == chunk.covers)
            .map_or(path.len(), |i| i + 1);
        let reflect_due =
            crate::memory::tokens(&path, crate::memory::reflect_mark(&path), end) >= m.settings.reflect_after;
        let pool_target = m.settings.pool_target;
        let (worker, session) = (m.worker.clone(), self.store.id().to_string());
        let notify = self.shared.clone();
        let covers = chunk.covers.clone();
        let task = tokio::spawn(async move {
            use crate::memory;
            let model = format!("{}/{}", worker.provider, worker.model);
            let mut run = ObserverRun {
                covers: covers.clone(),
                entries: vec![],
                error: None,
            };
            // Observer, then reflector when due, then dropper when the pool is over budget.
            match memory::observe(&worker, &known, &chunk, &session).await {
                Ok((notes, usage)) => {
                    let mut active = active;
                    active.extend(notes.iter().cloned());
                    run.entries.push(EntryKind::Notes {
                        notes,
                        covers: covers.clone(),
                        usage,
                        model: model.clone(),
                    });
                    if reflect_due {
                        match memory::reflect(&worker, &refl, &active, &session).await {
                            Ok((new, usage)) => {
                                let made = !new.is_empty();
                                let mut refl = refl;
                                refl.extend(new.iter().cloned());
                                run.entries.push(EntryKind::Reflections {
                                    reflections: new,
                                    covers,
                                    usage,
                                    model: model.clone(),
                                });
                                if made && memory::pool_tokens(&active) > pool_target {
                                    match memory::drop_notes(&worker, &refl, &active, pool_target, &session).await {
                                        Ok((ids, usage)) if !ids.is_empty() => {
                                            run.entries.push(EntryKind::Drops { ids, usage, model })
                                        }
                                        Ok(_) => {}
                                        Err(e) => run.error = Some(format!("dropper: {e}")),
                                    }
                                }
                            }
                            Err(e) => run.error = Some(format!("reflector: {e}")),
                        }
                    }
                }
                Err(e) => run.error = Some(e),
            }
            notify.notify.notify_one();
            run
        });
        if let Some(m) = self.memory.as_mut() {
            m.task = Some(task);
        }
    }

    /// Replace the conversation before a cut with the observer's notes (L4.5).
    /// The observer first catches up to the cut, so nothing before it goes
    /// unnoted. Returns whether anything was compacted.
    pub async fn compact(&mut self, why: &str) -> bool {
        use crate::memory;
        if self.memory.is_none() {
            return false;
        }
        self.collect_notes(true).await;
        let tokens_before = self.context_tokens();
        let Some(m) = self.memory.as_ref() else { return false };
        let (settings, worker, observe) = (m.settings.clone(), m.worker.clone(), m.observe);
        let (from, cut) = {
            let path = self.store.branch();
            let from = path
                .iter()
                .rev()
                .find_map(|e| match &e.kind {
                    EntryKind::Compaction { first_kept, .. } => path.iter().position(|x| &x.id == first_kept),
                    _ => None,
                })
                .unwrap_or(0);
            let Some(cut) = memory::cut_point(&path, from, settings.keep_recent) else {
                return false;
            };
            (from, path[cut].id.clone())
        };
        self.emit(AgentEvent::Compacting { tokens_before });
        self.emit(AgentEvent::Stream(StreamEvent::Notice {
            text: format!("compacting: {why}"),
        }));
        // Catch up to the cut.
        let mut failed = None;
        loop {
            let path = self.store.branch();
            let cut_i = path.iter().position(|e| e.id == cut).unwrap_or(path.len());
            let wm = memory::watermark(&path).max(from);
            if !observe || wm >= cut_i {
                break;
            }
            let chunk = memory::chunk(&path, wm, cut_i, settings.chunk_tokens);
            let known = memory::notes(&path);
            let covers = chunk.covers.clone();
            match memory::observe(&worker, &known, &chunk, self.store.id()).await {
                Ok((notes, usage)) => {
                    self.add(EntryKind::Notes {
                        notes,
                        covers,
                        usage,
                        model: format!("{}/{}", worker.provider, worker.model),
                    });
                }
                Err(e) => {
                    failed = Some(e);
                    break;
                }
            }
        }
        let path = self.store.branch();
        let cut_i = path.iter().position(|e| e.id == cut).unwrap_or(path.len());
        // The observer couldn't cover everything (or is off): the main model summarises the gap.
        let mut summary_text = None;
        if failed.is_some() || !observe {
            let wm = memory::watermark(&path).max(from);
            let gap = memory::chunk(&path, wm, cut_i, self.cfg.window_tokens / 2);
            if let Some(e) = &failed {
                self.emit(AgentEvent::Stream(StreamEvent::Notice {
                    text: format!(
                        "memory observer failed while catching up ({e}); summarising {}k tokens with the session model",
                        gap.tokens / 1000
                    ),
                }));
            }
            match memory::summarize(&self.adapter, &self.cfg.model, &gap, self.store.id()).await {
                Ok((t, _)) => summary_text = Some(t),
                Err(e) => self.emit(AgentEvent::Stream(StreamEvent::Notice {
                    text: format!("summary failed too ({e}); dropping the older messages"),
                })),
            }
        }
        let path = self.store.branch();
        let pos = |id: &str| path.iter().position(|e| e.id == id);
        let notes: Vec<memory::Note> = memory::active_notes(&path)
            .into_iter()
            .filter(|n| n.sources.iter().any(|s| pos(s).is_some_and(|i| i < cut_i)))
            .collect();
        let refl = memory::reflections(&path);
        let (read, changed) = memory::files(&path[..cut_i]);
        let dropped = path[from..cut_i]
            .iter()
            .filter(|e| matches!(e.kind, EntryKind::Message { .. }))
            .count();
        let mut summary = memory::render_summary(&refl, &notes, summary_text.as_deref(), &read, &changed, dropped);
        // The cut split a turn: the request being worked on stays word for word.
        if let Some(p) = memory::split_prompt(&path, cut_i) {
            summary.push_str(&format!(
                "\n\n## Request in progress\nThe conversation below continues the turn started by this user message:\n{p}"
            ));
        }
        let fallback = notes.is_empty() && refl.is_empty() && summary_text.is_none();
        let system = crate::prompt::render(&self.store.sections());
        let n = notes.len();
        self.add(EntryKind::Compaction {
            system,
            summary,
            first_kept: cut,
            tokens_before,
            notes: n,
        });
        self.compacted = true;
        self.usage_stale = true;
        // What plugins said at session start went with the compacted part.
        self.plugin_context("compact").await;
        let tokens_after = self.context_tokens();
        self.emit(AgentEvent::Compacted {
            tokens_before,
            tokens_after,
            notes: n,
            fallback,
        });
        self.update_report();
        true
    }

    pub fn with_quirks(mut self, q: Arc<Quirks>) -> Self {
        self.quirks = q;
        self
    }

    pub fn config(&self) -> &Config {
        &self.cfg
    }

    /// The model adapter the session started with.
    pub fn adapter(&self) -> &Arc<dyn Adapter> {
        &self.adapter
    }

    pub fn with_policy(mut self, policy: Policy) -> Self {
        self.policy = Arc::new(policy);
        self
    }

    /// The shared policy (mode presets change it at run time, L10.7).
    pub fn policy(&self) -> Arc<Policy> {
        self.policy.clone()
    }

    /// Share an existing policy (mode presets hold it too).
    pub fn with_shared_policy(mut self, policy: Arc<Policy>) -> Self {
        self.policy = policy;
        self
    }

    /// What `/reload` keeps: see [`Carry`].
    pub fn carry(&self) -> Carry {
        Carry {
            cwd: self.cfg.cwd.clone(),
            files: self.files.clone(),
            processes: self.processes.clone(),
            allow: self.policy.session_rules(),
            mode: self.modes.as_ref().and_then(|m| m.active()),
            started: self.plugins.as_ref().is_some_and(|p| p.started),
        }
    }

    /// Take over a reloaded session's live state (call after the other builder steps).
    pub fn adopt(mut self, c: Carry) -> Self {
        self.files = c.files;
        if c.processes.is_some() {
            self.processes = c.processes;
        }
        self.policy.allow_for_session(c.allow);
        if let (Some(m), Some(name)) = (&self.modes, c.mode.as_deref()) {
            // A preset removed from settings just ends.
            let _ = m.set(Some(name));
        }
        if let Some(p) = &mut self.plugins {
            p.started = c.started;
        }
        self
    }

    pub fn modes(&self) -> Option<&Arc<crate::modes::Modes>> {
        self.modes.as_ref()
    }

    /// Mode presets: their prompt section is applied at the start of each run.
    pub fn with_modes(mut self, modes: Arc<crate::modes::Modes>) -> Self {
        self.modes = Some(modes);
        self
    }

    pub fn with_approver(mut self, approver: Arc<dyn Approver>) -> Self {
        self.approver = Some(approver);
        self
    }

    /// The command judge (L6.8): risky commands the gate allows in auto mode go to it.
    pub fn with_judge(mut self, judge: Arc<crate::judge::Judge>) -> Self {
        judge.set_events(self.events.clone());
        self.judge = Some(judge);
        self
    }

    /// Override the first loop retry wait (default 2 s); tests use a short one.
    pub fn with_retry_base(mut self, base: Duration) -> Self {
        self.retry_base = base;
        self
    }

    /// Permission check for one prepared call (L6.1). `None` = run it; `Some` = the refusal to return.
    async fn authorize(
        &self,
        call: &ToolCall,
        read_only: bool,
        args: &serde_json::Value,
        hook: &crate::policy::HookDecision,
        cancel: &CancellationToken,
    ) -> Option<ToolOutput> {
        pipeline::authorize_cancellable(
            &self.policy,
            self.approver.as_ref(),
            self.judge.as_deref(),
            &call.name,
            read_only,
            args,
            &self.cfg.cwd,
            hook,
            cancel,
        )
        .await
    }

    /// Claude Code plugin hooks (L7.14): session start, prompt submit, stop, session end.
    pub fn with_plugin_hooks(mut self, hooks: Arc<crate::plugin_hooks::PluginHooks>) -> Self {
        let _ = self.shared.plugin_hooks.set(hooks.clone());
        self.plugins = Some(Plugins {
            hooks,
            started: false,
            stop_active: false,
        });
        self
    }

    /// Run `SessionStart` (`SubagentStart` in a subagent) and save what it says for the model.
    async fn plugin_context(&mut self, source: &str) {
        let Some(p) = &self.plugins else { return };
        let hooks = p.hooks.clone();
        for text in hooks.start(source).await {
            self.add(EntryKind::CustomMessage {
                kind: "hook".into(),
                text,
                display: false,
            });
        }
    }

    /// `SessionEnd` hooks of plugins, when the session closes.
    async fn plugin_end(&self) {
        if let Some(p) = &self.plugins {
            p.hooks.end().await;
        }
    }

    pub fn with_hooks(mut self, hooks: Arc<crate::hooks::Hooks>) -> Self {
        self.hooks = hooks;
        self
    }

    pub fn with_mcp(mut self, hub: Arc<crate::mcp::Hub>) -> Self {
        self.mcp = Some(hub);
        self
    }

    /// Script extensions: notices go to this session; `on(event)` handlers follow it.
    pub fn with_extensions(mut self, exts: Arc<crate::extensions::Extensions>) -> Self {
        let shared = self.shared.clone();
        exts.attach(
            self.events.clone(),
            Arc::new(move |name: &str, text: String, wake: bool| shared.remind(&format!("ext:{name}"), text, wake)),
        );
        exts.spawn_events(self.events.subscribe());
        let (a, b, c) = (self.shared.clone(), self.shared.clone(), self.shared.clone());
        let (d, e, f) = (self.shared.clone(), self.shared.clone(), self.shared.clone());
        exts.connect(crate::extensions::SessionApi {
            messages: Arc::new(move || a.ext.lock().unwrap().history.clone()),
            complete: Arc::new(
                move |system: Option<String>, messages: Vec<Message>, max: Option<u32>| {
                    let route = b.ext.lock().unwrap().route.clone();
                    Box::pin(async move {
                        let (adapter, mut req) = route.ok_or("complete: the session isn't ready yet")?;
                        req.messages = system
                            .map(|text| Message::System { text })
                            .into_iter()
                            .chain(messages)
                            .collect();
                        if let Some(m) = max {
                            req.max_tokens = req.max_tokens.min(m.max(1));
                        }
                        let mut rx = adapter.stream(req, CancellationToken::new());
                        let mut done = None;
                        while let Some(ev) = rx.recv().await {
                            if let StreamEvent::Done(m) | StreamEvent::Error(m) = ev {
                                done = Some(m);
                            }
                        }
                        let m = done.ok_or("complete: the provider stream ended without a result")?;
                        match m.stop {
                            StopReason::Error { .. } => Err(m.error_message.unwrap_or_else(|| "request failed".into())),
                            _ => Ok(m),
                        }
                    })
                },
            ),
            call_tool: Arc::new(move |name: String, args: serde_json::Value| {
                let tools = c.ext.lock().unwrap().tools.clone();
                Box::pin(async move {
                    use novacode_tools::Nested;
                    match tools {
                        Some((runner, ctx)) => runner.call(name, args, ctx).await,
                        None => ToolOutput::error("the session isn't ready yet"),
                    }
                })
            }),
            state_append: Arc::new(move |ext: &str, kind: &str, data: serde_json::Value| {
                let mut st = d.ext_state.lock().unwrap();
                let item = (ext.to_string(), kind.to_string(), data);
                st.live.push(item.clone());
                st.pending.push(item);
                drop(st);
                d.notify.notify_one();
            }),
            state_entries: Arc::new(move |ext: &str, kind: &str| {
                e.ext_state
                    .lock()
                    .unwrap()
                    .live
                    .iter()
                    .filter(|(x, k, _)| x == ext && k == kind)
                    .map(|(_, _, v)| v.clone())
                    .collect()
            }),
            send: Arc::new(move |text: String, steer: bool| {
                let mut q = f.queues.lock().unwrap();
                let content = vec![Block::text(text)];
                if steer {
                    q.steering.push_back(content);
                } else {
                    q.follow_up.push_back(content);
                }
                drop(q);
                f.notify.notify_one();
            }),
        });
        self.extensions = Some(exts);
        self
    }

    pub fn extensions(&self) -> Option<&Arc<crate::extensions::Extensions>> {
        self.extensions.as_ref()
    }

    /// The session's events, before it is spawned (hook observers).
    pub fn subscribe(&self) -> broadcast::Receiver<AgentEvent> {
        self.events.subscribe()
    }

    pub fn mcp(&self) -> Option<&Arc<crate::mcp::Hub>> {
        self.mcp.as_ref()
    }

    /// Everything codemode scripts may call besides the listed tools (L5.16).
    pub fn with_scripted(mut self, tools: Vec<Arc<dyn Tool>>) -> Self {
        self.scripted = tools;
        self
    }

    fn emit(&self, ev: AgentEvent) {
        let _ = self.events.send(ev); // no subscribers is fine
    }

    /// Undo this run's prompt when nothing else happened since `start`: only text
    /// prompts follow it on the branch. Moves the leaf back; returns their text.
    fn take_back(&mut self, start: Option<&str>) -> Option<String> {
        let start = start?;
        let branch = self.store.branch();
        let after = &branch[branch.iter().position(|e| e.id == start)? + 1..];
        let mut texts = Vec::new();
        for e in after {
            let EntryKind::Message {
                message: Message::User { content },
            } = &e.kind
            else {
                return None;
            };
            for b in content {
                let Block::Text { text, .. } = b else { return None };
                texts.push(text.clone());
            }
        }
        if texts.is_empty() {
            return None;
        }
        self.store.set_leaf(start).ok()?;
        self.update_report();
        Some(texts.join("\n"))
    }

    fn add(&mut self, kind: EntryKind) -> String {
        if let EntryKind::Message {
            message: Message::Assistant(a),
        } = &kind
            && a.usage.prompt_total() > 0
        {
            self.usage_stale = false;
        }
        let ev = match &kind {
            EntryKind::Message { message } => Some(message.clone()),
            EntryKind::CustomMessage { .. } => None,
            _ => None,
        };
        let id = match self.store.append(kind) {
            Ok(id) => {
                self.save_recovered();
                id
            }
            Err(e) => {
                // The entry is kept in memory and written by the next successful flush (L4.16).
                self.save_failed(&e.to_string());
                self.store.entries().last().map(|e| e.id.clone()).unwrap_or_default()
            }
        };
        if let Some(m) = ev {
            self.emit(AgentEvent::Message(m));
        }
        id
    }

    /// Tell the user once that the session file is behind.
    fn save_failed(&mut self, error: &str) {
        tracing::error!(error, "session write failed");
        if !std::mem::replace(&mut self.save_error, true) {
            self.emit(AgentEvent::Stream(StreamEvent::Notice {
                text: format!(
                    "session not saved: {error}. Nothing more runs until it saves; fix the cause (disk space, permissions), then send a message to retry."
                ),
            }));
        }
    }

    fn save_recovered(&mut self) {
        if std::mem::take(&mut self.save_error) {
            self.emit(AgentEvent::Stream(StreamEvent::Notice {
                text: "session saved again".into(),
            }));
        }
    }

    /// Everything is on disk, writing what a failed write left behind first.
    fn saved(&mut self) -> bool {
        if self.store.unsaved() == 0 {
            return true;
        }
        match self.store.flush() {
            Ok(()) => {
                self.save_recovered();
                true
            }
            Err(e) => {
                let e = e.to_string();
                tracing::error!(error = e, "session write failed again");
                self.emit(AgentEvent::Stream(StreamEvent::Notice {
                    text: format!("not run: the session can't be saved ({e})"),
                }));
                false
            }
        }
    }

    fn nudge(&mut self, text: &str) {
        self.add(EntryKind::CustomMessage {
            kind: "nudge".into(),
            text: text.into(),
            display: false,
        });
    }

    fn take_queued(&self, steering_only: bool) -> Vec<Vec<Block>> {
        let mut q = self.shared.queues.lock().unwrap();
        let mut out: Vec<Vec<Block>> = q.steering.drain(..).collect();
        if !steering_only {
            out.extend(q.follow_up.drain(..));
        }
        out
    }

    /// Run until the model stops and nothing is queued.
    pub async fn run(&mut self) {
        let next = self.shared.next_model.lock().unwrap().take();
        if let Some(s) = next {
            if (&s.provider, &s.model) != (&self.cfg.provider, &self.cfg.model) {
                // Recorded in the session (L2.11); the next request is a full cache miss.
                self.add(EntryKind::ModelChange {
                    provider: s.provider.clone(),
                    model: s.model.clone(),
                });
            }
            // Background model calls follow, so a model given up (e.g. at its limit) isn't used.
            if let Some(m) = self.memory.as_mut() {
                if !m.observe {
                    m.worker = crate::memory::Worker {
                        adapter: s.adapter.clone(),
                        provider: s.provider.clone(),
                        model: s.model.clone(),
                    };
                } else if let Some(h) = s.helper.clone() {
                    m.worker = h;
                }
            }
            if let (Some(j), Some(h)) = (&self.judge, &s.helper) {
                j.follow(h.clone());
            }
            self.adapter = s.adapter;
            self.cfg.provider = s.provider;
            self.cfg.model = s.model;
            self.cfg.window_tokens = s.window_tokens;
            self.cfg.max_tokens = s.max_tokens;
            self.cfg.images = s.images;
            self.cfg.reasoning = s.reasoning;
            let mut slot = self.shared.next_thinking.lock().unwrap();
            if slot.is_none() {
                *slot = Some(s.thinking);
            }
        }
        let next = self.shared.next_thinking.lock().unwrap().take();
        if let Some(level) = next
            && level != self.cfg.thinking
        {
            self.cfg.thinking = level;
            self.add(EntryKind::ThinkingChange { level });
        }
        let added: Vec<Arc<dyn Tool>> = std::mem::take(&mut *self.shared.new_tools.lock().unwrap());
        if !added.is_empty() {
            // Same name = the client's newer definition. The tool list changes: one cache miss mid-session.
            for t in added {
                self.tools.retain(|x| x.name() != t.name());
                self.tools.push(t);
            }
            self.defs = self.tools.iter().map(|t| pipeline::def(t.as_ref())).collect();
        }
        if let Some(m) = &self.modes {
            self.cfg.prompt.mode = m.prompt();
        }
        refresh_prompt(&mut self.store, &self.cfg.prompt);
        self.store_warm_usage();
        self.set_busy(true);
        self.update_report();
        let cancel = CancellationToken::new();
        *self.shared.cancel.lock().unwrap() = Some(cancel.clone());
        self.emit(AgentEvent::AgentStart);
        self.start_checkpoint();
        let mut rec = Recovery::default();
        let mut steering_only = false;
        if let Some(p) = self.plugins.as_mut()
            && !std::mem::replace(&mut p.started, true)
        {
            let resumed = self.store.branch().iter().any(|e| {
                matches!(
                    &e.kind,
                    EntryKind::Message {
                        message: Message::User { .. }
                    }
                )
            });
            self.plugin_context(if resumed { "resume" } else { "startup" }).await;
        }
        // Esc before the first reply takes the prompt back (like Claude Code): the leaf returns here.
        let start = self.store.branch().last().map(|e| e.id.clone());
        let mut first = true;
        let mut refused = false;
        let stop = loop {
            let queued = self.take_queued(steering_only);
            let mut added = queued.is_empty();
            for content in queued {
                // A user message ends an activated skill's turn rules (L5.25).
                self.policy.end_turn();
                // UserPromptSubmit (L7.14): a plugin may refuse the prompt or add context after it.
                let hooks = self.plugins.as_mut().map(|p| {
                    p.stop_active = false;
                    p.hooks.clone()
                });
                let extra = match hooks {
                    Some(h) => {
                        let text: Vec<&str> = content
                            .iter()
                            .filter_map(|b| match b {
                                Block::Text { text, .. } => Some(text.as_str()),
                                _ => None,
                            })
                            .collect();
                        match h.prompt(&text.join("\n")).await {
                            Ok(extra) => extra,
                            Err(why) => {
                                let why = if why.trim().is_empty() {
                                    "no reason given".into()
                                } else {
                                    why
                                };
                                self.emit(AgentEvent::Stream(StreamEvent::Notice {
                                    text: format!("a plugin hook refused the prompt: {why}"),
                                }));
                                continue;
                            }
                        }
                    }
                    None => vec![],
                };
                added = true;
                self.add(EntryKind::Message {
                    message: Message::User { content },
                });
                for text in extra {
                    self.add(EntryKind::CustomMessage {
                        kind: "hook".into(),
                        text,
                        display: false,
                    });
                }
            }
            if !added && first {
                refused = true;
                break StopReason::Stop;
            }
            self.flush_ext_state();
            let notes: Vec<(String, String)> = {
                let mut q = self.shared.queues.lock().unwrap();
                q.wake = false;
                q.reminders.drain(..).collect()
            };
            for (kind, text) in notes {
                self.add(EntryKind::CustomMessage {
                    kind,
                    text,
                    display: false,
                });
            }
            steering_only = true;
            // L4.16: no model call while the session file is missing entries.
            if !self.saved() {
                break StopReason::Error { kind: ErrorKind::Other };
            }
            // A long run: notes and compaction at this model-call boundary (L4.5).
            self.mid_run_memory().await;
            self.emit(AgentEvent::TurnStart);
            let msg = self.call_model(&cancel).await;
            if std::mem::take(&mut first)
                && msg.stop == StopReason::Aborted
                && msg.tool_calls().next().is_none()
                && msg.text().trim().is_empty()
                && let Some(text) = self.take_back(start.as_deref())
            {
                self.emit(AgentEvent::PromptTakenBack { text });
                break StopReason::Aborted;
            }
            let calls: Vec<ToolCall> = msg.tool_calls().cloned().collect();
            let stop = msg.stop.clone();
            let has_text = !msg.text().trim().is_empty();
            let text = msg.text();
            self.add(EntryKind::Message {
                message: Message::Assistant(msg),
            });
            // L4.16: and no tool runs either.
            if !calls.is_empty() && !self.saved() {
                self.skip_calls(&calls, "Not run: the session could not be saved.");
                break StopReason::Error { kind: ErrorKind::Other };
            }

            match &stop {
                StopReason::Aborted | StopReason::Error { .. } => {
                    self.skip_calls(&calls, "Not run: the reply ended early.");
                    break stop;
                }
                StopReason::Length if !calls.is_empty() => {
                    self.skip_calls(&calls, "Not run: your reply hit the output token limit before this call was complete. Make it smaller, for example split a large write into several edits.");
                    if rec.allow(Nudge::CutOff) {
                        continue;
                    }
                    break stop;
                }
                _ => {}
            }
            if !calls.is_empty() {
                let all_rejected = self.run_tools(calls, &cancel).await;
                if cancel.is_cancelled() {
                    break StopReason::Aborted;
                }
                if all_rejected && !rec.allow(Nudge::Malformed) {
                    break stop;
                }
                continue;
            }
            // No tool calls: recovery (L3.8), then queued follow-ups.
            let after_tools = matches!(
                self.store.projection().iter().rev().nth(1),
                Some(Message::ToolResult { .. })
            );
            let nudge = if stop == StopReason::Length {
                Some(Nudge::CutOff)
            } else if Recovery::looks_like_text_tool_call(&text) {
                Some(Nudge::TextToolCall)
            } else if !has_text && after_tools {
                Some(Nudge::EmptyAfterTools)
            } else {
                None
            };
            if let Some(n) = nudge
                && rec.allow(n)
            {
                let say = if n == Nudge::CutOff {
                    Nudge::cut_off(&text)
                } else {
                    n.text().into()
                };
                self.nudge(&say);
                continue;
            }
            if !self.shared.queues.lock().unwrap().follow_up.is_empty() {
                steering_only = false;
                continue;
            }
            break stop;
        };
        // Stop (L7.14): a plugin hook's block keeps the agent going with its reason.
        if stop == StopReason::Stop
            && !refused
            && let Some(p) = self.plugins.as_mut()
        {
            let (hooks, active) = (p.hooks.clone(), p.stop_active);
            let last = self
                .store
                .projection()
                .iter()
                .rev()
                .find_map(|m| match m {
                    Message::Assistant(a) => Some(a.text()),
                    _ => None,
                })
                .unwrap_or_default();
            if let Some(why) = hooks.stop(active, &last).await {
                if let Some(p) = self.plugins.as_mut() {
                    p.stop_active = true;
                }
                self.shared.remind("hook", why, true);
            }
        }
        *self.shared.cancel.lock().unwrap() = None;
        self.finish_checkpoint().await;
        self.set_busy(false);
        self.update_report();
        self.emit(AgentEvent::AgentEnd { stop });
        let q = self.shared.queues.lock().unwrap();
        if q.steering.is_empty() && q.follow_up.is_empty() && !q.wake {
            self.emit(AgentEvent::Settled);
        }
    }

    /// Synthetic results so every tool call has a result (history stays valid).
    fn skip_calls(&mut self, calls: &[ToolCall], why: &str) {
        for c in calls {
            let content = vec![Block::text(why)];
            let message = Message::ToolResult {
                call_id: c.id.clone(),
                tool_name: c.name.clone(),
                content,
                is_error: true,
                details: None,
            };
            self.add(EntryKind::Message { message });
        }
    }

    /// Extensions' `before_request` text for this call: sent after the cached
    /// prefix and never stored, so it costs no cache miss.
    async fn request_tail(&self) -> Vec<String> {
        let Some(exts) = &self.extensions else {
            return Vec::new();
        };
        let messages = self.store.projection();
        let last_user = messages.iter().rev().find_map(|m| match m {
            Message::User { content } => Some(
                content
                    .iter()
                    .filter_map(|b| match b {
                        Block::Text { text, .. } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n"),
            ),
            _ => None,
        });
        exts.before_request(
            &self.cfg.provider,
            &self.cfg.model,
            novacode_types::estimate::context(&messages),
            last_user,
        )
        .await
    }

    /// One model call with the overflow/retry state machine (L4.6).
    async fn call_model(&mut self, cancel: &CancellationToken) -> AssistantMessage {
        let tail = self.request_tail().await;
        let mut attempt = 0;
        let mut learned = 0;
        let mut compacted = false;
        loop {
            let route = format!("{}/{}", self.cfg.provider, self.cfg.model);
            let messages = self.store.projection();
            // Never ask for more output than the window has room for (L2.21), with 4096
            // spare for the estimate's error (pi's margin).
            let room = self
                .cfg
                .window_tokens
                .saturating_sub(novacode_types::estimate::context(&messages))
                .saturating_sub(4096);
            let req = Request {
                tail: tail.clone(),
                model: self.cfg.model.clone(),
                max_tokens: self.cfg.max_tokens.min(room.clamp(1024, u32::MAX as u64) as u32),
                messages,
                tools: self.quirks.apply(&route, &self.defs),
                cache: self.cfg.cache,
                thinking: self.cfg.reasoning.then_some(self.cfg.thinking),
                images: self.cfg.images,
                session: Some(self.store.id().to_string()),
            };
            let req_cancel = cancel.child_token();
            let mut rx = self.adapter.stream(req.clone(), req_cancel.clone());
            let (mut fin, mut looped) = (None, false);
            // Text and thinking per block, checked for a model loop every 1000 new bytes.
            let mut seen = std::collections::HashMap::<usize, (String, usize)>::new();
            while let Some(ev) = rx.recv().await {
                match ev {
                    StreamEvent::Done(m) | StreamEvent::Error(m) => fin = Some(m),
                    other => {
                        match &other {
                            StreamEvent::BlockStart {
                                index,
                                kind: novacode_types::BlockKind::Text | novacode_types::BlockKind::Thinking,
                                ..
                            } => {
                                seen.insert(*index, (String::new(), 0));
                            }
                            StreamEvent::Delta { index, text } if !looped => {
                                if let Some((buf, checked)) = seen.get_mut(index) {
                                    buf.push_str(text);
                                    if buf.len() >= *checked + 1000 {
                                        *checked = buf.len();
                                        looped = Recovery::looping(buf);
                                        if looped {
                                            req_cancel.cancel();
                                        }
                                    }
                                }
                            }
                            _ => {}
                        }
                        self.emit(AgentEvent::Stream(other))
                    }
                }
            }
            let m =
                fin.unwrap_or_else(|| failed(&self.cfg, ErrorKind::Other, "provider stream ended without a result"));
            // A looping reply is dropped and asked for again, like a dropped stream.
            let m = if looped && !cancel.is_cancelled() {
                failed(
                    &self.cfg,
                    ErrorKind::Transient,
                    "the reply kept repeating the same text (a model loop), so it was stopped",
                )
            } else {
                m
            };
            if !matches!(m.stop, StopReason::Error { .. } | StopReason::Aborted) {
                self.after_request(req, &m.usage);
                let mut side = self.shared.side.lock().unwrap();
                side.reply = Some(m.clone());
                side.thread.clear();
            }
            let StopReason::Error { kind } = &m.stop else { return m };
            // A tool schema the backend can't take (L2.20): strip what it named, resend at once.
            if *kind == ErrorKind::InvalidRequest
                && learned < 3
                && let Some(r) = m.error_message.as_deref().and_then(schema::rejected)
                && self.quirks.learn(&route, &r)
            {
                learned += 1;
                let text = m.error_message.clone().unwrap_or_default();
                self.hide_failed(m);
                self.emit(AgentEvent::Retry {
                    attempt: learned,
                    delay_ms: 0,
                    error: ErrorKind::InvalidRequest,
                    message: format!("tool schema rejected, retrying without it: {text}"),
                });
                continue;
            }
            // Too long for the model: compact, then retry once (L4.6).
            if *kind == ErrorKind::Overflow && !compacted && !cancel.is_cancelled() {
                compacted = true;
                self.hide_failed(m);
                if self.compact("the model rejected the request as too long").await {
                    self.emit(AgentEvent::Retry {
                        attempt: 1,
                        delay_ms: 0,
                        error: ErrorKind::Overflow,
                        message: "compacted, retrying".into(),
                    });
                    continue;
                }
                return failed(
                    &self.cfg,
                    ErrorKind::Overflow,
                    "the context is too long and nothing could be compacted",
                );
            }
            if !kind.is_retryable() || cancel.is_cancelled() {
                return m;
            }
            let server = match kind {
                ErrorKind::RateLimit {
                    retry_after_ms: Some(ms),
                } => Some(Duration::from_millis(*ms)),
                _ => None,
            };
            let Some(delay) = loop_delay(self.retry_base, attempt, server) else {
                return m;
            };
            let (kind, text) = (kind.clone(), m.error_message.clone().unwrap_or_default());
            self.hide_failed(m);
            attempt += 1;
            self.emit(AgentEvent::Retry {
                attempt,
                delay_ms: delay.as_millis() as u64,
                error: kind,
                message: text,
            });
            tokio::select! {
                _ = tokio::time::sleep(delay) => {}
                _ = cancel.cancelled() => return failed(&self.cfg, ErrorKind::Aborted, "aborted during retry wait"),
            }
        }
    }

    /// Keep a failed attempt on record, hidden from the model. It is asked for again,
    /// so clients drop the text it streamed.
    fn hide_failed(&mut self, m: AssistantMessage) {
        self.emit(AgentEvent::Stream(StreamEvent::RetryRollback));
        let id = self.add(EntryKind::Message {
            message: Message::Assistant(m),
        });
        self.add(EntryKind::ContextEdit {
            target: id,
            replace: None,
        });
    }

    /// Run one batch (L3.3): sequential preflight, then concurrent execution
    /// (or sequential if any tool asks), results appended in source order.
    /// Returns true when every call was rejected before running.
    /// A `read` of the same lines of a byte-identical file, whose earlier full result
    /// is still in the context: point at it instead of sending it again. After a
    /// compaction, rewind or change the earlier result is gone, so the file is sent.
    fn unchanged_read(&self, call: &ToolCall, mut out: ToolOutput) -> ToolOutput {
        let Some(sha) = out.details.as_ref().and_then(|d| d["sha"].as_str()) else {
            return out;
        };
        if call.name != "read" || out.is_error {
            return out;
        }
        let key = |a: &serde_json::Value| (a["path"].clone(), a["offset"].clone(), a["limit"].clone());
        let want = key(&call.arguments);
        let p = self.store.projection();
        let same: std::collections::HashSet<&str> = p
            .iter()
            .filter_map(|m| match m {
                Message::Assistant(a) => Some(a.tool_calls()),
                _ => None,
            })
            .flatten()
            .filter(|c| c.name == "read" && key(&c.arguments) == want)
            .map(|c| c.id.as_str())
            .collect();
        // Pointers carry no sha, so this finds the last full copy.
        let last = p.iter().rev().find_map(|m| match m {
            Message::ToolResult { call_id, details, .. } if same.contains(call_id.as_str()) => {
                details.as_ref()?["sha"].as_str()
            }
            _ => None,
        });
        if last == Some(sha) {
            let path = call.arguments["path"].as_str().unwrap_or("the file");
            out.content = vec![Block::text(format!(
                "{path} is unchanged since your last read of these lines; that result above is still current."
            ))];
            out.details = None;
        }
        out
    }

    async fn run_tools(&mut self, calls: Vec<ToolCall>, cancel: &CancellationToken) -> bool {
        let activated = if self.deferred.is_empty() {
            vec![]
        } else {
            self.activated()
        };
        let all: Vec<Arc<dyn Tool>> = self.tools.iter().chain(&self.deferred).cloned().collect();
        let mut prepared: Vec<(ToolCall, Prepared)> = calls
            .into_iter()
            .map(|c| match self.resolve_call(c, &activated) {
                Resolved::Deferred(c) => {
                    let p = pipeline::prepare(&all, &c);
                    (c, p)
                }
                Resolved::Listed(c) => {
                    let p = pipeline::prepare(&self.tools, &c);
                    (c, p)
                }
                Resolved::Locked(c) => {
                    let msg = format!("`{}` isn't available yet: find it with tool_search first", c.name);
                    (c, Prepared::Rejected(ToolOutput::error(msg)))
                }
            })
            .collect();
        // Malformed calls count toward the cap (L3.4); permission refusals don't.
        let all_rejected = prepared.iter().all(|(_, p)| matches!(p, Prepared::Rejected(_)));
        // What the command judge (L6.8) reads: the user's messages and recent calls, never replies or output.
        if let Some(j) = &self.judge {
            j.see(crate::judge::seen(&self.store.branch(), self.store.id()));
        }
        // Permissions are part of the sequential preflight (L3.3, L6.1); approvals are asked one at a time.
        for (call, p) in &mut prepared {
            // Hooks first (they may rewrite the args), so permissions judge what will run.
            let mut decision = crate::policy::HookDecision::None;
            if let Prepared::Ready(_, args, _) = p
                && !self.hooks.is_empty()
            {
                match self.hooks.before_tool(&call.name, args.clone()).await {
                    Ok((a, d)) => (*args, decision) = (a, d),
                    Err(out) => {
                        *p = Prepared::Rejected(out);
                        continue;
                    }
                }
            }
            if let Prepared::Ready(t, args, _) = p
                && let Some(refusal) = self.authorize(call, t.read_only(), args, &decision, cancel).await
            {
                *p = Prepared::Rejected(refusal);
            }
        }
        // No tool may change files before the prompt's checkpoint exists (L4.15).
        self.finish_checkpoint().await;
        self.pre_images(&prepared);
        let sequential = prepared
            .iter()
            .any(|(_, p)| matches!(p, Prepared::Ready(t, ..) if t.sequential()));
        let budget = Budget {
            window_tokens: self.cfg.window_tokens,
            used_tokens: novacode_types::estimate::context(&self.store.projection()),
        };
        let base = ToolCtx {
            cwd: self.cfg.cwd.clone(),
            env: self.env.clone(),
            cancel: cancel.clone(),
            updates: None,
            files: self.files.clone(),
            session_id: self.store.id().to_string(),
            processes: self.processes.clone(),
            native_tools: self.adapter.native_tool_changes(&self.cfg.model),
            nested: None,
            script: false,
        };
        let mut base = base;
        if prepared
            .iter()
            .any(|(_, p)| matches!(p, Prepared::Ready(t, ..) if t.name() == crate::codemode::NAME))
        {
            base.nested = Some(self.runner(budget));
        }
        let one = |call: &ToolCall, p: &Prepared| {
            let events = self.events.clone();
            let hooks = self.hooks.clone();
            let redactor = self.cfg.redactor.clone();
            let spill = self
                .cfg
                .spill_dir
                .join(format!("{}-{}.txt", base.session_id, sanitize(&call.id)));
            let mut ctx = base.clone();
            let call = call.clone();
            let ready = match p {
                Prepared::Ready(t, a, acc) => Ok((t.clone(), a.clone(), *acc)),
                Prepared::Rejected(o) => Err(o.clone()),
            };
            async move {
                let _ = events.send(AgentEvent::ToolStart { call: call.clone() });
                let t0 = Instant::now();
                let out = match ready {
                    _ if ctx.cancel.is_cancelled() => ToolOutput::error("Not run: cancelled by the user."),
                    Err(o) => o,
                    Ok((tool, args, accept)) => {
                        ctx.cancel = ctx.cancel.child_token();
                        let (tx, mut rx) = mpsc::channel::<String>(64);
                        ctx.updates = Some(tx);
                        let (ev2, id) = (events.clone(), call.id.clone());
                        tokio::spawn(async move {
                            while let Some(text) = rx.recv().await {
                                let _ = ev2.send(AgentEvent::ToolUpdate {
                                    call_id: id.clone(),
                                    text,
                                });
                            }
                        });
                        let started = hooks.start(&call.name, &ctx.cwd).await;
                        let mut out =
                            pipeline::run(tool.as_ref(), args.clone(), accept, &ctx, &spill, budget, &redactor).await;
                        hooks
                            .after_tool(&call.id, &call.name, &args, &ctx.cwd, started, &mut out)
                            .await;
                        out
                    }
                };
                let duration_ms = t0.elapsed().as_millis() as u64;
                let _ = events.send(AgentEvent::ToolEnd {
                    call_id: call.id.clone(),
                    is_error: out.is_error,
                    duration_ms,
                });
                (call, out)
            }
        };
        let results: Vec<(ToolCall, ToolOutput)> = if sequential {
            let mut v = Vec::new();
            for (c, p) in &prepared {
                v.push(one(c, p).await);
            }
            v
        } else {
            futures_util::future::join_all(prepared.iter().map(|(c, p)| one(c, p))).await
        };
        let mut found: Vec<String> = Vec::new();
        for (call, out) in results {
            if call.name == "tool_search"
                && let Some(names) = out.details.as_ref().and_then(|d| d["activate"].as_array())
            {
                found.extend(names.iter().filter_map(|n| n.as_str().map(String::from)));
            }
            let new_cwd = out
                .details
                .as_ref()
                .and_then(|d| d["set_cwd"].as_str())
                .filter(|_| !out.is_error)
                .map(PathBuf::from);
            let out = self.unchanged_read(&call, out);
            let message = Message::ToolResult {
                call_id: call.id,
                tool_name: call.name,
                content: out.content,
                is_error: out.is_error,
                details: out.details,
            };
            self.add(EntryKind::Message { message });
            if let Some(dir) = new_cwd {
                self.move_to(dir);
            }
        }
        // Announce found tools in-line (L4.8): the request's tool list never changes.
        if !found.is_empty() && self.adapter.native_tool_changes(&self.cfg.model) {
            let defs: Vec<ToolDef> = self
                .deferred
                .iter()
                .filter(|t| found.iter().any(|f| f == t.name()) && !activated.iter().any(|a| a == t.name()))
                .map(|t| pipeline::def(t.as_ref()))
                .collect();
            if !defs.is_empty() {
                self.add(EntryKind::Message {
                    message: Message::Custom {
                        kind: "tools_added".into(),
                        data: serde_json::json!({ "tools": defs }),
                    },
                });
            }
        }
        all_rejected
    }

    /// A tool moved the session (L5.22): tools, prompt sources, checkpoints and
    /// the process follow, and the model is told before its next call.
    fn move_to(&mut self, dir: PathBuf) {
        if !dir.is_dir() || dir == self.cfg.cwd {
            return;
        }
        let _ = std::env::set_current_dir(&dir);
        self.cfg.cwd = dir.clone();
        self.cfg.prompt.cwd = dir.clone();
        if self.checkpoints.is_some() {
            self.checkpoints = crate::checkpoint::resolve(&dir, &self.cfg.prompt.home);
        }
        self.add(EntryKind::Cwd { cwd: dir.clone() });
        let text = format!("The working directory is now {}.", dir.display());
        self.add(EntryKind::CustomMessage {
            kind: "env".into(),
            text: text.clone(),
            display: false,
        });
        self.emit(AgentEvent::Stream(StreamEvent::Notice { text }));
        self.emit(AgentEvent::CwdChanged {
            cwd: dir.display().to_string(),
            branch: crate::prompt::env_state(&dir).branch,
        });
    }
}

fn sanitize(id: &str) -> String {
    id.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Loop-level retries (L4.6), on top of the provider's own quick ones (L2.18), sized to outlast overload spells.
const LOOP_RETRIES: u32 = 4;
const LOOP_RETRY_BASE: Duration = Duration::from_secs(2);

/// Wait before loop retry `attempt` (0-based): base·2^attempt with up to 25% jitter down,
/// or the server's delay up to [`retry::MAX_RETRY_DELAY`]; `None` = give up.
fn loop_delay(base: Duration, attempt: u32, server: Option<Duration>) -> Option<Duration> {
    use std::hash::{BuildHasher, Hasher};
    if attempt >= LOOP_RETRIES {
        return None;
    }
    if let Some(d) = server {
        return (d <= retry::MAX_RETRY_DELAY).then_some(d);
    }
    // A fresh RandomState is randomly keyed: enough entropy for jitter without a new dependency.
    let r = std::collections::hash_map::RandomState::new().build_hasher().finish() as u16;
    let jitter = 1.0 - 0.25 * (r as f64 / u16::MAX as f64);
    Some(base.mul_f64(2f64.powi(attempt as i32) * jitter))
}

fn failed(cfg: &Config, kind: ErrorKind, msg: &str) -> AssistantMessage {
    let stop = if kind == ErrorKind::Aborted {
        StopReason::Aborted
    } else {
        StopReason::Error { kind }
    };
    AssistantMessage {
        content: vec![],
        stop,
        usage: Usage::default(),
        provider: cfg.provider.clone(),
        model: cfg.model.clone(),
        response_id: None,
        error_message: Some(msg.into()),
        thinking_level: None,
    }
}

fn log_err<T>(r: Result<T, crate::session::StoreError>) {
    if let Err(e) = r {
        tracing::error!(error = %e, "session write failed");
    }
}

/// Front-end side of a session (L8.1). Cheap to clone; all front-ends share one.
#[derive(Clone)]
pub struct SessionHandle {
    shared: Arc<Shared>,
    events: broadcast::Sender<AgentEvent>,
    pub session_id: String,
}

impl SessionHandle {
    pub fn subscribe(&self) -> broadcast::Receiver<AgentEvent> {
        self.events.subscribe()
    }
    /// Where the context goes, as of the last finished run.
    /// Prompts of the session as a tree, as of the last run (L4.3).
    pub fn prompts(&self) -> Vec<crate::session::PromptNode> {
        self.shared.prompts.lock().unwrap().clone()
    }

    pub fn context(&self) -> ContextReport {
        let mut r = self.shared.report.lock().unwrap().clone();
        // The warmer acts between runs: read its state now, not at the last run's end.
        let live = self.shared.warm.lock().unwrap().status.clone();
        if !r.warm.starts_with("off") && !live.is_empty() {
            r.warm = live;
        }
        r
    }
    /// Queue a prompt; delivered when the agent is idle or after the current run (follow-up).
    pub fn prompt(&self, text: impl Into<String>) {
        self.prompt_content(vec![Block::text(text.into())]);
    }
    /// A prompt with images or other blocks.
    pub fn prompt_content(&self, content: Vec<Block>) {
        self.shared.queues.lock().unwrap().follow_up.push_back(content);
        self.shared.notify.notify_one();
    }
    /// Delivered before the next model call of the running turn (L3.7).
    pub fn steer(&self, text: impl Into<String>) {
        self.steer_content(vec![Block::text(text.into())]);
    }
    /// A note for the model at its next call; `wake` starts a run when idle.
    pub fn remind(&self, text: impl Into<String>, wake: bool) {
        self.shared.remind("process", text.into(), wake);
    }

    /// A saved hidden note of `kind` (e.g. `agent` for subagent reports).
    pub fn remind_as(&self, kind: &str, text: impl Into<String>, wake: bool) {
        self.shared.remind(kind, text.into(), wake);
    }

    /// Show an event in every window (UI ops, notices); never stored.
    pub fn emit(&self, ev: AgentEvent) {
        let _ = self.events.send(ev);
    }

    pub fn steer_content(&self, content: Vec<Block>) {
        self.shared.queues.lock().unwrap().steering.push_back(content);
        self.shared.notify.notify_one();
    }
    pub fn abort(&self) {
        if let Some(c) = self.shared.cancel.lock().unwrap().as_ref() {
            c.cancel();
        }
    }
    /// The user activated a skill (`/skill:`): its `hooks` run from now on (L7.16).
    /// Returns what can't run.
    pub fn skill_activated(&self, s: &crate::resources::Skill) -> Vec<String> {
        self.shared
            .plugin_hooks
            .get()
            .map(|h| h.add_skill(s))
            .unwrap_or_default()
    }
    /// Switch model (any provider) from the next run on.
    pub fn set_model(&self, switch: Switch) {
        *self.shared.next_model.lock().unwrap() = Some(switch);
    }
    /// Tools run by an RPC client (L7.8), declared to the model from the next run on.
    pub fn add_tools(&self, tools: Vec<Arc<dyn Tool>>) {
        self.shared.new_tools.lock().unwrap().extend(tools);
    }
    /// Thinking level from the next run on (L2.16).
    pub fn set_thinking(&self, level: ThinkingLevel) {
        *self.shared.next_thinking.lock().unwrap() = Some(level);
    }
    /// Stop after the current run; the owner task then returns the agent.
    /// `/btw`: ask a side question on top of the conversation so far. Not stored,
    /// not seen by the main thread. It reuses the last request's bytes, so the
    /// cached prefix is read, not written. Follow-ups continue the side thread until
    /// the main thread makes its next request.
    pub async fn side_question(&self, question: &str) -> Result<String, String> {
        // shortcut: needs one model call in this process (a fresh resume has none yet).
        let (sent, adapter) = {
            let w = self.shared.warm.lock().unwrap();
            (self.shared.sent.lock().unwrap().clone(), w.adapter.clone())
        };
        let (Some(sent), Some(adapter)) = (sent, adapter) else {
            return Err("ask after the first reply of this session".into());
        };
        let (reply, thread) = {
            let side = self.shared.side.lock().unwrap();
            (side.reply.clone(), side.thread.clone())
        };
        let mut req = sent.req;
        // A reply that called tools can't be followed by a plain user turn.
        if let Some(r) = reply.filter(|r| r.stop != StopReason::ToolUse) {
            req.messages.push(Message::Assistant(r));
        }
        let note = "<system-reminder>\nThe user asks a side question (/btw) while you work. Answer it directly and briefly. Only read-only tools run here: read, grep, find, ls, and codemode scripts limited to those; any other call is refused. Use them only when the answer needs it. Neither the question nor your answer stays in the conversation.\n</system-reminder>";
        for (q, a) in &thread {
            req.messages.push(Message::user(q.clone()));
            req.messages.push(Message::Assistant(AssistantMessage {
                content: vec![Block::text(a.clone())],
                stop: StopReason::Stop,
                usage: Usage::default(),
                provider: String::new(),
                model: req.model.clone(),
                response_id: None,
                error_message: None,
                thinking_level: None,
            }));
        }
        req.messages.push(Message::user(format!("{note}\n\n{question}")));
        // Read-only runners: `top` answers the model's calls, `nested` serves codemode scripts.
        let tools = {
            let ext = self.shared.ext.lock().unwrap();
            ext.tools.as_ref().map(|(r, ctx)| {
                let ro: Vec<Arc<dyn Tool>> = r
                    .tools
                    .iter()
                    .filter(|t| SIDE_TOOLS.contains(&t.name()))
                    .cloned()
                    .collect();
                let nested = side_runner(r, ro.clone());
                let top = side_runner(r, ro.into_iter().chain(ext.codemode.clone()).collect());
                let mut ctx = ctx.clone();
                ctx.nested = Some(nested);
                (top, ctx)
            })
        };
        let mut round = 0;
        let answer = loop {
            // The tool list stays as sent, so the cached prefix still matches.
            let mut rx = adapter.stream(req.clone(), CancellationToken::new());
            let mut done = None;
            while let Some(ev) = rx.recv().await {
                if let StreamEvent::Done(m) | StreamEvent::Error(m) = ev {
                    done = Some(m);
                }
            }
            let m = done.ok_or("the provider stream ended without a result")?;
            if let StopReason::Error { .. } = m.stop {
                return Err(m.error_message.unwrap_or_else(|| "request failed".into()));
            }
            let calls: Vec<ToolCall> = m.tool_calls().cloned().collect();
            let Some((top, ctx)) = tools.as_ref().filter(|_| !calls.is_empty() && round < SIDE_ROUNDS) else {
                let mut answer = m.text().trim().to_string();
                if !calls.is_empty() {
                    answer.push_str("\n\n(stopped: it still wanted tools after the round limit)");
                }
                break answer;
            };
            round += 1;
            req.messages.push(Message::Assistant(m));
            use novacode_tools::Nested;
            let outs = futures_util::future::join_all(
                calls
                    .iter()
                    .map(|c| top.call(c.name.clone(), c.arguments.clone(), ctx.clone())),
            )
            .await;
            for (c, out) in calls.into_iter().zip(outs) {
                req.messages.push(Message::ToolResult {
                    call_id: c.id,
                    tool_name: c.name,
                    content: out.content,
                    is_error: out.is_error,
                    details: None,
                });
            }
        };
        self.shared
            .side
            .lock()
            .unwrap()
            .thread
            .push((question.into(), answer.clone()));
        Ok(answer)
    }

    /// Name the session (shown by `/resume` and `novacode agents`); applied between runs.
    pub fn rename(&self, name: impl Into<String>) {
        *self.shared.rename.lock().unwrap() = Some(name.into());
        self.shared.notify.notify_one();
    }

    /// Move the session to another folder (`/worktree`), as `enter_worktree` does; applied between runs.
    pub fn move_to(&self, dir: PathBuf) {
        *self.shared.move_to.lock().unwrap() = Some(dir);
        self.shared.notify.notify_one();
    }

    /// Take back the newest queued message that hasn't reached the model yet (pi's
    /// Alt+Up). `steer` picks the steering queue, else the follow-ups.
    pub fn dequeue(&self, steer: bool) -> bool {
        let mut q = self.shared.queues.lock().unwrap();
        let q = if steer { &mut q.steering } else { &mut q.follow_up };
        q.pop_back().is_some()
    }

    /// Take every queued follow-up that hasn't started out of the queue (user abort, L3.7).
    /// Their texts come back so the window can return them to the editor, whoever queued them.
    // shortcut: follow-ups holding images stay queued (the editor can't take them back); fine until image follow-ups are common.
    pub fn take_follow_ups(&self) -> Vec<String> {
        let mut q = self.shared.queues.lock().unwrap();
        let text_only = |c: &Vec<Block>| c.iter().all(|b| matches!(b, Block::Text { .. }));
        let (taken, kept): (VecDeque<_>, VecDeque<_>) = q.follow_up.drain(..).partition(text_only);
        q.follow_up = kept;
        taken
            .into_iter()
            .map(|c| {
                c.into_iter()
                    .filter_map(|b| match b {
                        Block::Text { text, .. } => Some(text),
                        _ => None,
                    })
                    .collect()
            })
            .collect()
    }

    /// Compact now, or as soon as the running turn ends (`/compact`, L4.5).
    pub fn compact(&self) {
        *self.shared.compact.lock().unwrap() = true;
        self.shared.notify.notify_one();
    }

    pub fn shutdown(&self) {
        *self.shared.closed.lock().unwrap() = true;
        self.shared.notify.notify_one();
    }
}

/// Process manager → loop events and reminders for the model (L5.11).
async fn process_bridge(
    mut rx: broadcast::Receiver<novacode_tools::process::Event>,
    handle: SessionHandle,
    events: broadcast::Sender<AgentEvent>,
) {
    use novacode_tools::process::{Attention, Event};
    loop {
        let ev = rx.recv().await;
        // A reloaded session's manager lives on in the new one; this bridge is done.
        if *handle.shared.closed.lock().unwrap() {
            break;
        }
        match ev {
            Ok(Event::Started { id, name }) => {
                let _ = events.send(AgentEvent::ProcessStarted { id, name });
            }
            Ok(Event::Ended { id, name, end }) => {
                let _ = events.send(AgentEvent::ProcessEnded {
                    id,
                    name,
                    code: end.code,
                    stopped: end.stopped,
                });
            }
            Ok(Event::Notice { text, on, .. }) => match on {
                Attention::Wake => handle.remind(text, true),
                Attention::Context => handle.remind(text, false),
                Attention::Ignore => {}
            },
            Err(broadcast::error::RecvError::Lagged(_)) => {}
            Err(broadcast::error::RecvError::Closed) => break,
        }
    }
}

/// Start the owner task. It is the only code that changes the session.
pub fn spawn(agent: Agent) -> (SessionHandle, tokio::task::JoinHandle<Agent>) {
    // Every builder step is done: extensions can see and call the session from now on.
    agent.update_report();
    let handle = SessionHandle {
        shared: agent.shared.clone(),
        events: agent.events.clone(),
        session_id: agent.store.id().into(),
    };
    if agent.cfg.metrics {
        let id = agent.store.id().to_string();
        let header = crate::metrics::Record::Session {
            ts: crate::session::now_ms(),
            session: id.clone(),
            cwd: agent.cfg.cwd.display().to_string(),
            version: env!("CARGO_PKG_VERSION").into(),
            window: agent.cfg.window_tokens,
        };
        tokio::spawn(crate::metrics::record(
            agent.events.subscribe(),
            crate::metrics::path(&agent.cfg.prompt.home, &id),
            header,
            agent.cfg.redactor.clone(),
        ));
    }
    if let Some(pm) = &agent.processes {
        tokio::spawn(process_bridge(
            pm.events.subscribe(),
            handle.clone(),
            agent.events.clone(),
        ));
    }
    if agent.cfg.warm {
        tokio::spawn(warmer(agent.shared.clone(), agent.events.clone()));
    }
    let task = tokio::spawn(async move {
        let mut agent = agent;
        loop {
            let pending = {
                let q = agent.shared.queues.lock().unwrap();
                !q.steering.is_empty() || !q.follow_up.is_empty() || q.wake
            };
            agent.flush_ext_state();
            agent.collect_notes(false).await;
            if let Some(name) = agent.shared.rename.lock().unwrap().take() {
                log_err(
                    agent
                        .store
                        .append(crate::session::EntryKind::SessionInfo { name: name.clone() }),
                );
                agent.emit(AgentEvent::Stream(StreamEvent::Notice {
                    text: format!("session named \"{name}\""),
                }));
            }
            let dir = agent.shared.move_to.lock().unwrap().take();
            if let Some(dir) = dir {
                agent.move_to(dir);
            }
            if std::mem::take(&mut *agent.shared.compact.lock().unwrap()) {
                if agent.memory.is_none() {
                    agent.emit(AgentEvent::Stream(StreamEvent::Notice {
                        text: "compaction is off (memory.enabled)".into(),
                    }));
                } else if !agent.compact("you asked for it").await {
                    agent.emit(AgentEvent::Stream(StreamEvent::Notice {
                        text: "nothing to compact: the conversation fits in the part kept verbatim".into(),
                    }));
                }
                continue;
            }
            if pending {
                agent.run().await;
                let idle = {
                    let q = agent.shared.queues.lock().unwrap();
                    q.steering.is_empty() && q.follow_up.is_empty() && !q.wake
                };
                if idle {
                    agent.after_settle().await;
                }
            } else if *agent.shared.closed.lock().unwrap() {
                agent.shared.warm_wake.notify_one();
                agent.store_warm_usage();
                agent.plugin_end().await;
                break agent;
            } else {
                agent.shared.notify.notified().await;
            }
        }
    });
    (handle, task)
}

/// Bring the model's prompt up to date by appending only (L4.7, L1.7): the full
/// prompt on a new session; later, a delta of the sections that changed and a
/// reminder when the day, branch or HEAD moved. Sessions from before sections
/// existed keep their prompt as it is.
fn refresh_prompt(store: &mut Store, src: &crate::prompt::Sources) {
    use crate::prompt;
    let have = store.sections();
    let fresh = store.entries().is_empty();
    if have.is_empty() && !fresh {
        return;
    }
    let now = prompt::env_state(&src.cwd);
    let environment = match have.iter().find(|(n, _)| n == "environment") {
        // Kept as written at start: only its body is passed back in.
        Some((_, t)) => t
            .trim_start_matches("<environment>\n")
            .trim_end_matches("\n</environment>")
            .to_string(),
        None => prompt::environment(&src.cwd, &now),
    };
    let want = prompt::build(src, &environment);
    let patch = prompt::diff(&have, &want);
    if !patch.is_empty() {
        let text = if fresh {
            prompt::render(&want)
        } else {
            prompt::render_update(&patch)
        };
        log_err(store.append(EntryKind::Sections { patch }));
        log_err(store.append(EntryKind::Message {
            message: Message::System { text },
        }));
    }
    match store.last_env() {
        Some(old) if old == now => {}
        Some(old) => {
            if let Some(text) = prompt::reminder(&old, &now) {
                log_err(store.append(EntryKind::CustomMessage {
                    kind: "env".into(),
                    text,
                    display: false,
                }));
            }
            log_err(store.append(env_entry(now)));
        }
        None => log_err(store.append(env_entry(now))),
    }
}

fn env_entry(e: crate::prompt::EnvState) -> EntryKind {
    EntryKind::Env {
        date: e.date,
        branch: e.branch,
        head: e.head,
    }
}

/// Replays the last request with `max_tokens: 1` shortly before its cache expires,
/// while the expected saving beats the cost (L2.17). Stops after 60 minutes of
/// work or 30 idle minutes since the last real request (pi's caps).
async fn warmer(shared: Arc<Shared>, events: broadcast::Sender<AgentEvent>) {
    use crate::cache;
    loop {
        if *shared.closed.lock().unwrap() {
            return;
        }
        let plan = {
            let sent = shared.sent.lock().unwrap().clone();
            let w = shared.warm.lock().unwrap();
            match (sent, &w.adapter, w.stopped) {
                (Some(s), Some(a), false) => a
                    .warm_ttl(&s.req)
                    .and_then(cache::warm_delay)
                    .map(|d| (s.at + d, w.generation, a.clone(), s)),
                _ => None,
            }
        };
        let Some((when, generation, adapter, sent)) = plan else {
            shared.warm_wake.notified().await;
            continue;
        };
        tokio::select! {
            _ = tokio::time::sleep_until(when.into()) => {}
            _ = shared.warm_wake.notified() => continue,
        }
        let (chance, tokens) = {
            let mut w = shared.warm.lock().unwrap();
            if w.generation != generation || *shared.closed.lock().unwrap() {
                continue;
            }
            let age = w.started.map(|s| s.elapsed()).unwrap_or_default();
            let cap = Duration::from_secs(if w.busy { 3600 } else { 1800 });
            let chance = cache::return_chance(w.busy, w.idle_warms);
            let tokens = sent.usage.prompt_total();
            let saving = cache::warm_saving(tokens, sent.req.cache, chance);
            let stop = if age > cap {
                Some(format!("stopped: {} since the last request", cache::human(age)))
            } else if saving < cache::WARM_THRESHOLD {
                Some(format!(
                    "stopped: a refresh of {tokens} tokens at {:.0}% return chance isn't worth it",
                    chance * 100.0
                ))
            } else {
                None
            };
            if let Some(why) = stop {
                w.stopped = true;
                w.status = why;
                continue;
            }
            w.status = "refreshing".into();
            (chance, tokens)
        };
        let mut req = sent.req.clone();
        req.max_tokens = 1;
        let mut rx = adapter.stream(req, CancellationToken::new());
        let mut done = None;
        while let Some(ev) = rx.recv().await {
            if let StreamEvent::Done(m) = ev {
                done = Some(m);
            }
        }
        let mut w = shared.warm.lock().unwrap();
        match done {
            Some(m) if w.generation == generation => {
                if !w.busy {
                    w.idle_warms += 1;
                }
                w.status = format!(
                    "kept {} tokens warm ({:.0}% return chance), next in about {}",
                    m.usage.cache_read,
                    chance * 100.0,
                    adapter
                        .warm_ttl(&sent.req)
                        .and_then(cache::warm_delay)
                        .map(cache::human)
                        .unwrap_or_default()
                );
                w.usage.push((sent.provider.clone(), sent.req.model.clone(), m.usage));
                if let Some(s) = shared.sent.lock().unwrap().as_mut() {
                    s.at = Instant::now();
                }
                let _ = events.send(AgentEvent::Stream(StreamEvent::Notice {
                    text: format!("cache kept warm: {} of {tokens} prompt tokens read", m.usage.cache_read),
                }));
            }
            Some(_) => {}
            None => {
                w.stopped = true;
                w.status = "stopped: the warm request failed".into();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `/btw` runner keeps its own user-wait clock, apart from the main one.
    #[test]
    fn side_runner_has_its_own_wait_clock() {
        let dir = tempfile::tempdir().unwrap();
        let main = crate::codemode::Runner {
            tools: vec![],
            policy: Arc::new(Policy::new(Permissions::default(), dir.path().into(), None)),
            approver: None,
            spill_dir: dir.path().into(),
            redactor: Redactor::default(),
            budget: Budget {
                window_tokens: 100_000,
                used_tokens: 0,
            },
            seq: Default::default(),
            hooks: Default::default(),
            judge: None,
            wait: Default::default(),
        };
        let side = side_runner(&main, vec![]);
        let _waiting = main.wait.begin();
        std::thread::sleep(Duration::from_millis(20));
        assert!(main.wait.now() >= Duration::from_millis(20));
        assert_eq!(side.wait.now(), Duration::ZERO);
    }
}
