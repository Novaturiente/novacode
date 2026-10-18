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
    /// `None` = nobody to ask (print mode, headless): asks become refusals or justifications.
    approver: Option<Arc<dyn Approver>>,
    /// Learned tool-schema rejections (L2.20).
    quirks: Arc<Quirks>,
    /// Observer and compaction (L4.5); `None` = compaction off.
    memory: Option<Memory>,
    /// A compaction happened since the last request: its miss is deliberate.
    compacted: bool,
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
    worker: crate::memory::Worker,
    settings: crate::memory::Settings,
    /// The background observer run, if one is going.
    task: Option<tokio::task::JoinHandle<ObserverRun>>,
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
    reminders: VecDeque<String>,
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
    /// What the context holds, refreshed after each run (for `/context`).
    report: Mutex<ContextReport>,
    /// The last request that touched the cache, for the miss detector and the warmer (L2.17).
    sent: Mutex<Option<crate::cache::Sent>>,
    warm: Mutex<Warm>,
    warm_wake: Notify,
    /// `/compact` was asked for; the owner compacts when idle.
    compact: Mutex<bool>,
    /// The session's prompts as a tree, refreshed with the report (for `/tree`).
    prompts: Mutex<Vec<crate::session::PromptNode>>,
}

/// Warmer state, reset by every real request.
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
        let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
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
            approver: None,
            quirks: Arc::new(Quirks::ephemeral()),
            memory: None,
            compacted,
            checkpoints: None,
            processes: None,
            deferred: Vec::new(),
            scripted: Vec::new(),
            mcp: None,
            hooks: Arc::default(),
            extensions: None,
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
            worker,
            settings,
            task: None,
        });
        self
    }

    fn context_tokens(&self) -> u64 {
        novacode_types::estimate::context(&self.store.projection())
    }

    /// Store a finished observer run; with `wait`, wait for a running one.
    async fn collect_notes(&mut self, wait: bool) {
        let Some(m) = self.memory.as_mut() else { return };
        let Some(task) = m.task.take_if(|t| wait || t.is_finished()) else {
            return;
        };
        let Ok(run) = task.await else { return };
        // A branch switch while it ran: the notes belong to another branch.
        if self.store.branch().iter().any(|e| e.id == run.covers) {
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
        if m.task.is_some() {
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
        let (settings, worker) = (m.settings.clone(), m.worker.clone());
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
        self.emit(AgentEvent::Stream(StreamEvent::Notice {
            text: format!("compacting: {why}"),
        }));
        // Catch up to the cut.
        let mut failed = None;
        loop {
            let path = self.store.branch();
            let cut_i = path.iter().position(|e| e.id == cut).unwrap_or(path.len());
            let wm = memory::watermark(&path).max(from);
            if wm >= cut_i {
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
        // The observer couldn't cover everything: the main model summarises the gap.
        let mut summary_text = None;
        if let Some(e) = &failed {
            let wm = memory::watermark(&path).max(from);
            let gap = memory::chunk(&path, wm, cut_i, self.cfg.window_tokens / 2);
            self.emit(AgentEvent::Stream(StreamEvent::Notice {
                text: format!(
                    "memory observer failed while catching up ({e}); summarising {}k tokens with the session model",
                    gap.tokens / 1000
                ),
            }));
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
        let summary = memory::render_summary(&refl, &notes, summary_text.as_deref(), &read, &changed, dropped);
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

    pub fn with_policy(mut self, policy: Policy) -> Self {
        self.policy = Arc::new(policy);
        self
    }

    pub fn with_approver(mut self, approver: Arc<dyn Approver>) -> Self {
        self.approver = Some(approver);
        self
    }

    /// Permission check for one prepared call (L6.1). `None` = run it; `Some` = the refusal to return.
    async fn authorize(&self, call: &ToolCall, args: &serde_json::Value) -> Option<ToolOutput> {
        pipeline::authorize(&self.policy, self.approver.as_ref(), &call.name, args, &self.cfg.cwd).await
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
        exts.attach(self.events.clone());
        exts.spawn_events(self.events.subscribe());
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

    fn add(&mut self, kind: EntryKind) -> String {
        let ev = match &kind {
            EntryKind::Message { message } => Some(message.clone()),
            EntryKind::CustomMessage { .. } => None,
            _ => None,
        };
        let id = match self.store.append(kind) {
            Ok(id) => id,
            Err(e) => {
                // The entry is kept in memory; the session file misses it. Surface, don't stop.
                tracing::error!(error = %e, "session write failed");
                self.store.entries().last().map(|e| e.id.clone()).unwrap_or_default()
            }
        };
        if let Some(m) = ev {
            self.emit(AgentEvent::Message(m));
        }
        id
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
        let stop = loop {
            for content in self.take_queued(steering_only) {
                self.add(EntryKind::Message {
                    message: Message::User { content },
                });
            }
            let notes: Vec<String> = {
                let mut q = self.shared.queues.lock().unwrap();
                q.wake = false;
                q.reminders.drain(..).collect()
            };
            for text in notes {
                self.add(EntryKind::CustomMessage {
                    kind: "process".into(),
                    text,
                    display: false,
                });
            }
            steering_only = true;
            // A long run near the window: compact at this model-call boundary (L4.5).
            if let Some(m) = &self.memory
                && self.context_tokens() >= self.cfg.window_tokens * 95 / 100
                && m.settings.compact_at < self.cfg.window_tokens
            {
                self.compact("the context is nearly full").await;
            }
            self.emit(AgentEvent::TurnStart);
            let msg = self.call_model(&cancel).await;
            let calls: Vec<ToolCall> = msg.tool_calls().cloned().collect();
            let stop = msg.stop.clone();
            let has_text = !msg.text().trim().is_empty();
            let text = msg.text();
            self.add(EntryKind::Message {
                message: Message::Assistant(msg),
            });

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
                self.nudge(n.text());
                continue;
            }
            if !self.shared.queues.lock().unwrap().follow_up.is_empty() {
                steering_only = false;
                continue;
            }
            break stop;
        };
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

    /// One model call with the overflow/retry state machine (L4.6).
    async fn call_model(&mut self, cancel: &CancellationToken) -> AssistantMessage {
        let mut attempt = 0;
        let mut learned = 0;
        let mut compacted = false;
        loop {
            let route = format!("{}/{}", self.cfg.provider, self.cfg.model);
            let messages = self.store.projection();
            // Never ask for more output than the window has room for (L2.21).
            let room = self
                .cfg
                .window_tokens
                .saturating_sub(novacode_types::estimate::context(&messages));
            let req = Request {
                model: self.cfg.model.clone(),
                max_tokens: self.cfg.max_tokens.min(room.clamp(1024, u32::MAX as u64) as u32),
                messages,
                tools: self.quirks.apply(&route, &self.defs),
                cache: self.cfg.cache,
                thinking: self.cfg.reasoning.then_some(self.cfg.thinking),
                images: self.cfg.images,
                session: Some(self.store.id().to_string()),
            };
            let mut rx = self.adapter.stream(req.clone(), cancel.child_token());
            let mut fin = None;
            while let Some(ev) = rx.recv().await {
                match ev {
                    StreamEvent::Done(m) | StreamEvent::Error(m) => fin = Some(m),
                    other => self.emit(AgentEvent::Stream(other)),
                }
            }
            let m =
                fin.unwrap_or_else(|| failed(&self.cfg, ErrorKind::Other, "provider stream ended without a result"));
            if !matches!(m.stop, StopReason::Error { .. } | StopReason::Aborted) {
                self.after_request(req, &m.usage);
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
            let Some(delay) = retry::delay(attempt, server) else {
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

    /// Keep a failed attempt on record, hidden from the model.
    fn hide_failed(&mut self, m: AssistantMessage) {
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
        // Permissions are part of the sequential preflight (L3.3, L6.1); approvals are asked one at a time.
        for (call, p) in &mut prepared {
            // Hooks first (they may rewrite the args), so permissions judge what will run.
            if let Prepared::Ready(_, args, _) = p
                && !self.hooks.is_empty()
            {
                match self.hooks.before_tool(&call.name, args.clone()).await {
                    Ok(a) => *args = a,
                    Err(out) => {
                        *p = Prepared::Rejected(out);
                        continue;
                    }
                }
            }
            if let Prepared::Ready(_, args, _) = p
                && let Some(refusal) = self.authorize(call, args).await
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
        };
        let mut base = base;
        if prepared
            .iter()
            .any(|(_, p)| matches!(p, Prepared::Ready(t, ..) if t.name() == crate::codemode::NAME))
        {
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
            base.nested = Some(Arc::new(crate::codemode::Runner {
                tools,
                policy: self.policy.clone(),
                approver: self.approver.clone(),
                spill_dir: self.cfg.spill_dir.clone(),
                redactor: self.cfg.redactor.clone(),
                budget,
                seq: Default::default(),
                hooks: self.hooks.clone(),
            }));
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
                        let mut out =
                            pipeline::run(tool.as_ref(), args.clone(), accept, &ctx, &spill, budget, &redactor).await;
                        hooks.after_tool(&call.name, &args, &mut out).await;
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
        let mut q = self.shared.queues.lock().unwrap();
        q.reminders.push_back(text.into());
        q.wake |= wake;
        drop(q);
        self.shared.notify.notify_one();
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
    /// Switch model (any provider) from the next run on.
    pub fn set_model(&self, switch: Switch) {
        *self.shared.next_model.lock().unwrap() = Some(switch);
    }
    /// Thinking level from the next run on (L2.16).
    pub fn set_thinking(&self, level: ThinkingLevel) {
        *self.shared.next_thinking.lock().unwrap() = Some(level);
    }
    /// Stop after the current run; the owner task then returns the agent.
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
        match rx.recv().await {
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
            agent.collect_notes(false).await;
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
