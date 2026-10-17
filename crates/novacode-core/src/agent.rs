//! The agent loop (L3.1) and the session owner task (L8.1, L0.2).
//!
//! One state machine: take queued input → call the model (retrying per L4.6)
//! → run tools → decide whether to continue. It never fails: errors end the
//! run as an `AgentEnd` with the stop reason, and every effect is an appended
//! session entry plus an event.

use crate::pipeline::{self, Prepared};
use crate::policy::{Approval, ApprovalRequest, Approver, Permissions, Policy, Verdict};
use crate::recovery::{Nudge, Recovery};
use crate::session::{EntryKind, Store};
use novacode_ai::adapter::{Adapter, CacheRetention, Request};
use novacode_ai::retry;
use novacode_base::redact::Redactor;
use novacode_tools::guard::Budget;
use novacode_tools::{ExecutionEnv, FileState, Tool, ToolCtx, ToolOutput};
use novacode_types::{
    AgentEvent, AssistantMessage, Block, ErrorKind, Message, StopReason, StreamEvent, ToolCall, ToolDef, Usage,
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
    /// Where cut tool output is spilled in full.
    pub spill_dir: PathBuf,
    /// Masks novacode's own secrets in tool output (L6.6).
    pub redactor: Redactor,
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
    last_usage: Usage,
    policy: Arc<Policy>,
    /// `None` = nobody to ask (print mode, headless): asks become refusals or justifications.
    approver: Option<Arc<dyn Approver>>,
}

#[derive(Default)]
struct Queues {
    steering: VecDeque<String>,
    follow_up: VecDeque<String>,
}

#[derive(Default)]
struct Shared {
    queues: Mutex<Queues>,
    /// The running turn's token; a stale abort can't hit the next run (L3.6).
    cancel: Mutex<Option<CancellationToken>>,
    notify: Notify,
    closed: Mutex<bool>,
}

impl Agent {
    pub fn new(
        adapter: Arc<dyn Adapter>,
        tools: Vec<Arc<dyn Tool>>,
        mut store: Store,
        cfg: Config,
        env: Arc<dyn ExecutionEnv>,
    ) -> Self {
        if store.entries().is_empty() {
            let text = crate::prompt::system_prompt(&cfg.cwd);
            log_err(store.append(EntryKind::Message {
                message: Message::System { text },
            }));
        }
        if store.last_model() != Some((cfg.provider.clone(), cfg.model.clone())) {
            log_err(store.append(EntryKind::ModelChange {
                provider: cfg.provider.clone(),
                model: cfg.model.clone(),
            }));
        }
        let defs = tools
            .iter()
            .map(|t| ToolDef {
                name: t.name().into(),
                description: t.description().into(),
                schema: pipeline::schema(t.as_ref()),
            })
            .collect();
        let (events, _) = broadcast::channel(EVENT_BUFFER);
        let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
        Self {
            adapter,
            tools,
            defs,
            store,
            cfg,
            env,
            files: Arc::new(FileState::default()),
            events,
            shared: Arc::default(),
            last_usage: Usage::default(),
            policy: Arc::new(Policy::new(Permissions::default(), home, None)),
            approver: None,
        }
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
        let verdict = self
            .policy
            .check(&call.name, args, &self.cfg.cwd, self.approver.is_some());
        let (reason, rule, risky) = match verdict {
            Verdict::Allow => return None,
            Verdict::Deny(why) => return Some(ToolOutput::error(why)),
            Verdict::Ask { reason, rule, risky } => (reason, rule, risky),
        };
        let Some(approver) = &self.approver else {
            return Some(ToolOutput::error(format!(
                "Not run: this needs the user's approval ({reason}) and nobody can approve it here. Add an allow rule or run interactively."
            )));
        };
        let req = ApprovalRequest {
            tool: call.name.clone(),
            args: args.clone(),
            reason,
            rule,
            risky,
        };
        let answer = approver.approve(req.clone()).await;
        self.policy.record(&req, &answer);
        match answer {
            Approval::Deny(why) => Some(ToolOutput::error(match why {
                Some(w) if !w.trim().is_empty() => format!("The user declined this call: {w}"),
                _ => "The user declined this call.".into(),
            })),
            _ => None,
        }
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

    fn take_queued(&self, steering_only: bool) -> Vec<String> {
        let mut q = self.shared.queues.lock().unwrap();
        let mut out: Vec<String> = q.steering.drain(..).collect();
        if !steering_only {
            out.extend(q.follow_up.drain(..));
        }
        out
    }

    /// Run until the model stops and nothing is queued.
    pub async fn run(&mut self) {
        let cancel = CancellationToken::new();
        *self.shared.cancel.lock().unwrap() = Some(cancel.clone());
        self.emit(AgentEvent::AgentStart);
        let mut rec = Recovery::default();
        let mut steering_only = false;
        let stop = loop {
            for text in self.take_queued(steering_only) {
                self.add(EntryKind::Message {
                    message: Message::user(text),
                });
            }
            steering_only = true;
            self.emit(AgentEvent::TurnStart);
            let msg = self.call_model(&cancel).await;
            self.last_usage = msg.usage;
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
        self.emit(AgentEvent::AgentEnd { stop });
        let q = self.shared.queues.lock().unwrap();
        if q.steering.is_empty() && q.follow_up.is_empty() {
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
        loop {
            let req = Request {
                model: self.cfg.model.clone(),
                messages: self.store.projection(),
                tools: self.defs.clone(),
                max_tokens: self.cfg.max_tokens,
                cache: self.cfg.cache,
            };
            let mut rx = self.adapter.stream(req, cancel.child_token());
            let mut fin = None;
            while let Some(ev) = rx.recv().await {
                match ev {
                    StreamEvent::Done(m) | StreamEvent::Error(m) => fin = Some(m),
                    other => self.emit(AgentEvent::Stream(other)),
                }
            }
            let m =
                fin.unwrap_or_else(|| failed(&self.cfg, ErrorKind::Other, "provider stream ended without a result"));
            let StopReason::Error { kind } = &m.stop else { return m };
            // shortcut: Overflow is surfaced; compact-then-retry-once arrives with compaction (M7, L4.5).
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
            // Keep the failed attempt on record, hidden from the model.
            let (kind, text) = (kind.clone(), m.error_message.clone().unwrap_or_default());
            let id = self.add(EntryKind::Message {
                message: Message::Assistant(m),
            });
            self.add(EntryKind::ContextEdit {
                target: id,
                replace: None,
            });
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

    /// Run one batch (L3.3): sequential preflight, then concurrent execution
    /// (or sequential if any tool asks), results appended in source order.
    /// Returns true when every call was rejected before running.
    async fn run_tools(&mut self, calls: Vec<ToolCall>, cancel: &CancellationToken) -> bool {
        let mut prepared: Vec<(ToolCall, Prepared)> = calls
            .into_iter()
            .map(|c| {
                let p = pipeline::prepare(&self.tools, &c);
                (c, p)
            })
            .collect();
        // Malformed calls count toward the cap (L3.4); permission refusals don't.
        let all_rejected = prepared.iter().all(|(_, p)| matches!(p, Prepared::Rejected(_)));
        // Permissions are part of the sequential preflight (L3.3, L6.1); approvals are asked one at a time.
        for (call, p) in &mut prepared {
            if let Prepared::Ready(_, args, _) = p
                && let Some(refusal) = self.authorize(call, args).await
            {
                *p = Prepared::Rejected(refusal);
            }
        }
        let sequential = prepared
            .iter()
            .any(|(_, p)| matches!(p, Prepared::Ready(t, ..) if t.sequential()));
        let budget = Budget {
            window_tokens: self.cfg.window_tokens,
            used_tokens: self.last_usage.prompt_total() + self.last_usage.output,
        };
        let base = ToolCtx {
            cwd: self.cfg.cwd.clone(),
            env: self.env.clone(),
            cancel: cancel.clone(),
            updates: None,
            files: self.files.clone(),
            session_id: self.store.id().to_string(),
        };
        let one = |call: &ToolCall, p: &Prepared| {
            let events = self.events.clone();
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
                        pipeline::run(tool.as_ref(), args, accept, &ctx, &spill, budget, &redactor).await
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
        for (call, out) in results {
            let message = Message::ToolResult {
                call_id: call.id,
                tool_name: call.name,
                content: out.content,
                is_error: out.is_error,
                details: out.details,
            };
            self.add(EntryKind::Message { message });
        }
        all_rejected
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
    /// Queue a prompt; delivered when the agent is idle or after the current run (follow-up).
    pub fn prompt(&self, text: impl Into<String>) {
        self.shared.queues.lock().unwrap().follow_up.push_back(text.into());
        self.shared.notify.notify_one();
    }
    /// Delivered before the next model call of the running turn (L3.7).
    pub fn steer(&self, text: impl Into<String>) {
        self.shared.queues.lock().unwrap().steering.push_back(text.into());
        self.shared.notify.notify_one();
    }
    pub fn abort(&self) {
        if let Some(c) = self.shared.cancel.lock().unwrap().as_ref() {
            c.cancel();
        }
    }
    /// Stop after the current run; the owner task then returns the agent.
    pub fn shutdown(&self) {
        *self.shared.closed.lock().unwrap() = true;
        self.shared.notify.notify_one();
    }
}

/// Start the owner task. It is the only code that changes the session.
pub fn spawn(agent: Agent) -> (SessionHandle, tokio::task::JoinHandle<Agent>) {
    let handle = SessionHandle {
        shared: agent.shared.clone(),
        events: agent.events.clone(),
        session_id: agent.store.id().into(),
    };
    let task = tokio::spawn(async move {
        let mut agent = agent;
        loop {
            let pending = {
                let q = agent.shared.queues.lock().unwrap();
                !q.steering.is_empty() || !q.follow_up.is_empty()
            };
            if pending {
                agent.run().await;
            } else if *agent.shared.closed.lock().unwrap() {
                break agent;
            } else {
                agent.shared.notify.notified().await;
            }
        }
    });
    (handle, task)
}
