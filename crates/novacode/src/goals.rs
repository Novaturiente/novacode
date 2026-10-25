//! Goals (`/goal`): an objective with checkable completion criteria that the
//! agent keeps working on by itself until it is met.
//!
//! Built for novacode from Claude Code's and Codex's `/goal` (pi has none):
//! - `/goal <text>` sets the goal at once (the harness, not the model) and starts
//!   a turn; the model's first act is to propose sharper criteria (`goal update`).
//! - The plan is the existing `todo` list; a goal adds the objective, the
//!   criteria and the loop.
//! - After each settled run a judge (the small model, or a classifier model with
//!   `goal.judge`) reads the conversation and says met, not met (with a reason fed
//!   back) or impossible, like Claude Code. "Met" goes to the `auditor` subagent,
//!   which re-checks every criterion in the workspace before the goal closes.
//! - Continuation is one saved hidden reminder per settled run (cache-safe).
//! - The loop stops itself: Esc pauses, two quiet runs pause as stalled,
//!   `goal.maxRuns` and `goal.maxTokens` cap it, errors pause or retry; runs
//!   that end with background work running are not judged (its report wakes it).
//! - State is one JSON file per session, so it survives a restart.

use crate::agents::Supervisor;
use futures_util::future::BoxFuture;
use novacode_base::storage::{self, Durability};
use novacode_core::agent::SessionHandle;
use novacode_tools::{Tool, ToolCtx, ToolOutput};
use novacode_types::ui::{Node, Time, UiOp};
use novacode_types::{AgentEvent, Block, Message, StopReason};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    Active,
    /// Needs the user (the agent called `block`).
    Blocked,
    /// Stopped by the user, a stall, a limit or an error; `/goal resume` continues.
    Paused,
    Auditing,
    Done,
    /// The judge found it can't be met.
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Goal {
    pub objective: String,
    pub criteria: Vec<String>,
    /// Steps (the todo list) are done in order, one per run.
    #[serde(default)]
    pub ordered: bool,
    pub state: State,
    /// Why it's blocked, paused or failed, or the auditor's feedback.
    #[serde(default)]
    pub note: Option<String>,
    pub audit: bool,
    pub started_ms: u64,
    #[serde(default)]
    pub ended_ms: Option<u64>,
    /// Automatic continuations since it started or was resumed.
    #[serde(default)]
    pub runs: u32,
    #[serde(default)]
    stalls: u32,
    /// Tokens spent (prompt + output) while it was active.
    #[serde(default)]
    pub tokens: u64,
    /// The judge's latest "not met yet" reason.
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    retries: u32,
}

impl Goal {
    fn new(objective: String, criteria: Vec<String>, ordered: bool, audit: bool) -> Self {
        Goal {
            objective,
            criteria,
            ordered,
            state: State::Active,
            note: None,
            audit,
            started_ms: novacode_core::session::now_ms(),
            ended_ms: None,
            runs: 0,
            stalls: 0,
            tokens: 0,
            reason: None,
            retries: 0,
        }
    }
}

/// Who decides whether a goal is met after each run.
pub enum Judge {
    /// A model that reads the conversation: the observer's model by default (it follows
    /// `/model`), or the `provider/model` set in `goal.judge` (`fixed`: it stays).
    Model {
        worker: Mutex<novacode_core::memory::Worker>,
        fixed: bool,
    },
    /// A classifier model (`goal.judge`, e.g. `typesafe/jev-latest`): calibrated
    /// probabilities instead of a reason; "met" needs at least 0.8.
    Classifier(
        Arc<novacode_core::classifier::Classify>,
        &'static novacode_ai::classify::Model,
    ),
}

pub struct Config {
    pub max_runs: u32,
    pub max_tokens: Option<u64>,
    pub audit: bool,
    pub audit_model: Option<String>,
    pub judge: Option<Judge>,
}

#[derive(Debug, PartialEq)]
enum Verdict {
    Met(String),
    NotMet(String),
    Impossible(String),
}

/// What the judge sees of the conversation: the newest ~24k characters.
const RECENT_CHARS: usize = 24_000;

pub struct Goals {
    path: PathBuf,
    goal: Mutex<Option<Goal>>,
    sup: Option<Arc<Supervisor>>,
    cfg: Config,
    handle: OnceLock<SessionHandle>,
    procs: OnceLock<Arc<novacode_tools::process::ProcessManager>>,
    recent: Mutex<VecDeque<String>>,
}

impl Goals {
    /// After a model switch: the default judge uses the new provider's small model.
    pub fn set_judge_model(&self, w: novacode_core::memory::Worker) {
        if let Some(Judge::Model { worker, fixed: false }) = &self.cfg.judge {
            *worker.lock().unwrap() = w;
        }
    }

    pub fn new(path: PathBuf, sup: Option<Arc<Supervisor>>, cfg: Config) -> Arc<Self> {
        let goal = storage::read_json::<Goal>(&path).ok().flatten();
        Arc::new(Goals {
            path,
            goal: Mutex::new(goal),
            sup,
            cfg,
            handle: OnceLock::new(),
            procs: OnceLock::new(),
            recent: Mutex::default(),
        })
    }

    pub fn current(&self) -> Option<Goal> {
        self.goal.lock().unwrap().clone()
    }

    /// Change the goal, save it and show it in the Agent tab and status line.
    fn update<R>(&self, f: impl FnOnce(&mut Option<Goal>) -> R) -> R {
        let mut g = self.goal.lock().unwrap();
        let r = f(&mut g);
        let saved = match g.as_ref() {
            Some(goal) => {
                let _ = std::fs::create_dir_all(self.path.parent().unwrap_or(&self.path));
                storage::write_json(&self.path, goal, Durability::Fast)
            }
            None => std::fs::remove_file(&self.path).or_else(|e| match e.kind() {
                std::io::ErrorKind::NotFound => Ok(()),
                _ => Err(e),
            }),
        };
        if let Err(e) = saved {
            tracing::warn!(%e, "goal state");
        }
        let shown = g.clone();
        drop(g);
        self.publish(shown.as_ref());
        r
    }

    fn node(&self, g: &Goal) -> Node {
        let status = match g.state {
            State::Active => "running",
            State::Auditing | State::Blocked => "waiting",
            State::Paused => "stopped",
            State::Done => "done",
            State::Failed => "failed",
        };
        let mut detail: Vec<String> = g.criteria.iter().map(|c| format!("· {c}")).collect();
        let what = match g.state {
            State::Active => format!("working · run {}/{}", g.runs, self.cfg.max_runs),
            State::Auditing => "the auditor is checking".into(),
            State::Blocked => "blocked: needs you".into(),
            State::Paused => "paused · /goal resume".into(),
            State::Done => "done".into(),
            State::Failed => "can't be met".into(),
        };
        detail.push(match &g.note {
            Some(n) => format!("{what}: {}", n.lines().next().unwrap_or("")),
            None => what,
        });
        if let Some(r) = g.reason.as_ref().filter(|_| g.state == State::Active) {
            detail.push(format!("next: {}", r.lines().next().unwrap_or("")));
        }
        detail.push(format!(
            "{} tokens · enter status · p pause · r resume · e edit · x clear",
            tokens(g.tokens)
        ));
        let actions = [
            ("enter", "/goal"),
            ("p", "/goal pause"),
            ("r", "/goal resume"),
            ("e", "/goal edit"),
            ("x", "/goal clear"),
        ]
        .into_iter()
        .map(|(k, a)| (k.to_string(), a.to_string()))
        .collect();
        Node {
            id: "goal".into(),
            parent: None,
            label: g.objective.lines().next().unwrap_or("").to_string(),
            tag: None,
            status: Some(status.into()),
            icon: None,
            dim: matches!(g.state, State::Done | State::Failed),
            detail,
            time: Some(match g.ended_ms {
                Some(e) => Time {
                    ms: Some(e.saturating_sub(g.started_ms)),
                    since: None,
                },
                None => Time {
                    ms: None,
                    since: Some(g.started_ms),
                },
            }),
            order: Some(0.0),
            actions,
        }
    }

    fn publish(&self, g: Option<&Goal>) {
        let Some(h) = self.handle.get() else { return };
        let op = match g {
            Some(g) => UiOp::Publish {
                tab: "agent".into(),
                source: "goal".into(),
                node: Box::new(self.node(g)),
            },
            None => UiOp::Remove {
                tab: "agent".into(),
                source: "goal".into(),
                id: None,
            },
        };
        h.emit(AgentEvent::Ui { ext: "goal".into(), op });
        // The status-line marker (`ext` segment) while a goal is open.
        let (text, style) = match g.map(|g| g.state) {
            Some(State::Active) => (
                Some(format!("◎ goal {}/{}", g.unwrap().runs, self.cfg.max_runs)),
                "accent",
            ),
            Some(State::Auditing) => (Some("◎ goal auditing".to_string()), "accent"),
            Some(State::Paused) => (Some("◎ goal paused".to_string()), "warn"),
            Some(State::Blocked) => (Some("◎ goal blocked".to_string()), "warn"),
            _ => (None, "dim"),
        };
        let op = UiOp::Status {
            key: "goal".into(),
            text,
            style: Some(style.into()),
        };
        h.emit(AgentEvent::Ui { ext: "goal".into(), op });
    }

    fn notice(&self, text: String) {
        if let Some(h) = self.handle.get() {
            h.emit(AgentEvent::Stream(novacode_types::StreamEvent::Notice { text }));
        }
    }

    /// Keep the goal going: after each settled run, a saved reminder wakes the
    /// agent again until the goal is done, failed, blocked, paused or out of budget.
    pub fn watch(
        self: Arc<Self>,
        handle: SessionHandle,
        procs: Option<Arc<novacode_tools::process::ProcessManager>>,
    ) -> tokio::task::JoinHandle<()> {
        let mut rx = handle.subscribe();
        let _ = self.handle.set(handle.clone());
        if let Some(p) = procs {
            let _ = self.procs.set(p);
        }
        self.publish(self.current().as_ref());
        tokio::spawn(async move {
            let mut tools = 0u32;
            let mut stop = None;
            loop {
                let ev = match rx.recv().await {
                    Ok(ev) => ev,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(_) => return,
                };
                match ev {
                    AgentEvent::AgentStart => tools = 0,
                    AgentEvent::ToolStart { .. } => tools += 1,
                    AgentEvent::Message(m) => self.observe(&m),
                    AgentEvent::AgentEnd { stop: s } => {
                        if s == StopReason::Aborted && self.pause_if_active("you interrupted it") {
                            self.notice("goal paused: you interrupted it (/goal resume continues)".into());
                        }
                        stop = Some(s);
                    }
                    AgentEvent::Settled => {
                        if let Some(text) = self.after_run(tools, stop.take()).await {
                            handle.remind_as("goal", text, true);
                        }
                    }
                    _ => {}
                }
            }
        })
    }

    /// Count tokens and keep the newest conversation for the judge.
    fn observe(&self, m: &Message) {
        if !self.current().is_some_and(|g| g.state == State::Active) {
            return;
        }
        if let Message::Assistant(a) = m {
            let spent = a.usage.prompt_total() + a.usage.output;
            if spent > 0 {
                let mut g = self.goal.lock().unwrap();
                if let Some(g) = g.as_mut() {
                    g.tokens += spent;
                }
            }
        }
        let line = render(m);
        if line.is_empty() {
            return;
        }
        let mut r = self.recent.lock().unwrap();
        r.push_back(line);
        while r.iter().map(String::len).sum::<usize>() > RECENT_CHARS && r.len() > 1 {
            r.pop_front();
        }
    }

    fn pause_if_active(&self, why: &str) -> bool {
        self.update(|g| match g {
            Some(g) if g.state == State::Active => {
                g.state = State::Paused;
                g.note = Some(why.into());
                true
            }
            _ => false,
        })
    }

    fn pause(&self, why: String) {
        if self.pause_if_active(&why) {
            self.notice(format!("goal paused: {why} (/goal resume continues)"));
        }
    }

    /// Subagents or background processes still running: their reports wake the agent.
    fn background(&self) -> bool {
        self.sup.as_ref().is_some_and(|s| s.pending() > 0) || self.procs.get().is_some_and(|p| p.running() > 0)
    }

    /// After a settled run: the continuation text, or `None` (and maybe a new state).
    async fn after_run(&self, tools: u32, stop: Option<StopReason>) -> Option<String> {
        let g = self.current().filter(|g| g.state == State::Active)?;
        // Errors (Claude Code's policy): retry what clears by itself, pause on the rest.
        if let Some(StopReason::Error { kind }) = &stop {
            if !kind.is_retryable() {
                self.pause(format!("the turn failed ({kind:?}); fix the cause"));
                return None;
            }
            let n = self.update(|g| {
                g.as_mut().map(|g| {
                    g.retries += 1;
                    g.retries
                })
            })?;
            if n > 3 {
                self.pause(format!("the turn failed 3 times in a row ({kind:?})"));
                return None;
            }
            self.notice(format!(
                "goal still active: the turn failed ({kind:?}); retrying in {}s",
                15 * n
            ));
            tokio::time::sleep(Duration::from_secs(15 * n as u64)).await;
            return self
                .current()
                .filter(|g| g.state == State::Active)
                .map(|g| continuation(&g));
        }
        if g.retries > 0 {
            self.update(|g| g.as_mut().map(|g| g.retries = 0));
        }
        if let Some(max) = self.cfg.max_tokens
            && g.tokens >= max
        {
            self.pause(format!("reached goal.maxTokens ({})", tokens(max)));
            return Some(format!(
                "The goal's token budget is used up, so the goal is paused: {}\nDon't start new work. Summarise what is done, what is left, what blocks it, and the next useful step.",
                g.objective
            ));
        }
        if self.background() {
            return None;
        }
        let verdict = match &self.cfg.judge {
            Some(j) => match self.judge(j, &g).await {
                Ok(v) => Some(v),
                Err(e) => {
                    tracing::warn!(%e, "goal judge");
                    None
                }
            },
            None => None,
        };
        match verdict {
            Some(Verdict::Met(why)) => return self.met(&g, &format!("The judge found the goal met: {why}")).await,
            Some(Verdict::Impossible(why)) => {
                self.update(|g| {
                    if let Some(g) = g {
                        g.state = State::Failed;
                        g.note = Some(why.clone());
                        g.ended_ms = Some(novacode_core::session::now_ms());
                    }
                });
                self.notice(format!("goal can't be met: {why}"));
                return None;
            }
            Some(Verdict::NotMet(why)) => {
                self.update(|g| g.as_mut().map(|g| g.reason = Some(why)));
            }
            None => {}
        }
        self.next_step(tools)
    }

    /// Stall and run checks, then the continuation.
    fn next_step(&self, tools: u32) -> Option<String> {
        let max_runs = self.cfg.max_runs;
        let (text, pause) = self.update(|g| {
            let g = g.as_mut().filter(|g| g.state == State::Active)?;
            g.stalls = if tools == 0 { g.stalls + 1 } else { 0 };
            let why = if g.stalls >= 2 {
                Some("stalled: two runs in a row without a tool call".to_string())
            } else if g.runs >= max_runs {
                Some(format!("reached goal.maxRuns ({max_runs})"))
            } else {
                None
            };
            if let Some(w) = why {
                g.state = State::Paused;
                g.note = Some(w.clone());
                return Some((None, Some(w)));
            }
            g.runs += 1;
            Some((Some(continuation(g)), None))
        })?;
        if let Some(w) = pause {
            self.notice(format!("goal paused: {w} (/goal resume continues)"));
        }
        text
    }

    async fn judge(&self, j: &Judge, g: &Goal) -> Result<Verdict, String> {
        let convo = self
            .recent
            .lock()
            .unwrap()
            .iter()
            .cloned()
            .collect::<Vec<_>>()
            .join("\n");
        let goal = goal_text(g);
        match j {
            Judge::Model { worker, .. } => {
                let w = worker.lock().unwrap().clone();
                let req = novacode_ai::adapter::Request {
                    tail: Vec::new(),
                    model: w.model.clone(),
                    messages: vec![
                        Message::System {
                            text: JUDGE_PROMPT.into(),
                        },
                        Message::User {
                            content: vec![Block::text(format!(
                                "{goal}\n\nThe conversation since the goal started (newest last):\n{convo}"
                            ))],
                        },
                    ],
                    tools: vec![],
                    max_tokens: 400,
                    cache: novacode_ai::adapter::CacheRetention::Short,
                    thinking: None,
                    images: false,
                    session: None,
                };
                let mut rx = w.adapter.stream(req, tokio_util::sync::CancellationToken::new());
                while let Some(ev) = rx.recv().await {
                    match ev {
                        novacode_types::StreamEvent::Done(m) => {
                            return verdict(&m.text()).ok_or_else(|| format!("unreadable verdict: {}", m.text()));
                        }
                        novacode_types::StreamEvent::Error(m) => {
                            return Err(m.error_message.unwrap_or_else(|| format!("{:?}", m.stop)));
                        }
                        _ => {}
                    }
                }
                Err("the judge's stream ended without a reply".into())
            }
            Judge::Classifier(c, m) => {
                use novacode_ai::classify::{Answer, Context, Question};
                let criteria = [
                    (
                        "met",
                        "every completion criterion is shown met by evidence in the conversation",
                    ),
                    (
                        "not_met",
                        "some criterion is not shown met yet, but more work could get there",
                    ),
                    ("impossible", "the goal can never be met as stated"),
                ]
                .into_iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect();
                let mut state = serde_json::Map::new();
                state.insert("goal".into(), json!(goal));
                state.insert("conversation".into(), json!(convo));
                let ctx = Context {
                    state,
                    images: vec![],
                    questions: [(
                        "verdict".to_string(),
                        Question::Choice {
                            instructions: "Has the coding agent met the goal, judged only from the conversation?"
                                .into(),
                            criteria,
                        },
                    )]
                    .into(),
                };
                let out = c.run(m, &ctx).await?;
                if out.stop_reason == "error" {
                    return Err(out.error_message.unwrap_or_else(|| "classifier error".into()));
                }
                let Some(Answer::Choice { probabilities, .. }) = out.answers.get("verdict") else {
                    return Err("the classifier gave no verdict".into());
                };
                let p = |k: &str| probabilities.get(k).copied().unwrap_or(0.0);
                Ok(if p("met") >= 0.8 {
                    Verdict::Met(format!("classifier: met p={:.2}", p("met")))
                } else if p("impossible") >= 0.8 {
                    Verdict::Impossible(format!("classifier: impossible p={:.2}", p("impossible")))
                } else {
                    Verdict::NotMet(format!("classifier: met p={:.2}, not shown yet", p("met")))
                })
            }
        }
    }

    /// The goal looks met: the auditor re-checks it in the workspace when it's on.
    /// Returns the continuation when the auditor rejects it.
    async fn met(&self, g: &Goal, evidence: &str) -> Option<String> {
        match self.audit(g, evidence).await {
            Ok(note) => {
                self.notice(format!("goal done: {}", g.objective.lines().next().unwrap_or("")));
                self.finish(note);
                None
            }
            Err(feedback) => {
                self.update(|x| {
                    if let Some(x) = x {
                        x.state = State::Active;
                        x.note = Some(feedback.clone());
                    }
                });
                self.current().map(|g| continuation(&g))
            }
        }
    }

    fn finish(&self, note: Option<String>) {
        self.update(|cur| {
            if let Some(x) = cur {
                x.state = State::Done;
                x.note = note;
                x.ended_ms = Some(novacode_core::session::now_ms());
            }
        });
    }

    /// Ok(note) when it may close, Err(feedback) when the auditor (or its failure) keeps it open.
    async fn audit(&self, g: &Goal, evidence: &str) -> Result<Option<String>, String> {
        let Some(sup) = self.sup.as_ref().filter(|_| g.audit) else {
            return Ok(None);
        };
        self.update(|cur| cur.as_mut().map(|x| x.state = State::Auditing));
        let task = format!(
            "Goal: {}\n\nCompletion criteria:\n{}\n\nThe agent's evidence:\n{evidence}",
            g.objective,
            g.criteria
                .iter()
                .map(|c| format!("- {c}"))
                .collect::<Vec<_>>()
                .join("\n")
        );
        match sup
            .audit(task, "audit the goal".into(), self.cfg.audit_model.clone())
            .await
        {
            Ok(r) if approved(&r) => Ok(Some("approved by the auditor".into())),
            Ok(r) => Err(format!(
                "the auditor rejected completion; keep working on what it found:\n{}",
                r.lines().skip(1).collect::<Vec<_>>().join("\n").trim()
            )),
            Err(e) => Err(format!(
                "the audit couldn't run ({e}); the goal stays open. The user can turn it off with /goal audit off."
            )),
        }
    }

    /// `/goal <text>`: set it now, from the user's words.
    pub fn start(&self, objective: String) {
        self.recent.lock().unwrap().clear();
        let audit = self.cfg.audit && self.sup.is_some();
        self.update(|g| *g = Some(Goal::new(objective, vec![], false, audit)));
    }

    /// `/goal edit` fills the input with this, to change and send back.
    pub fn edit_text(&self) -> Option<String> {
        self.current().map(|g| format!("/goal edit {}", goal_text(&g)))
    }

    /// `/goal pause|resume|clear|edit <text>|audit on|off`, or the status text for `/goal`.
    /// `Err` for anything else (the host treats it as a new objective).
    pub fn command(&self, arg: &str) -> Result<String, String> {
        match arg {
            "" | "status" => Ok(self
                .current()
                .map_or("no goal; /goal <what you want done> starts one".into(), |g| status(&g))),
            "pause" => self.update(|g| match g {
                Some(g) if matches!(g.state, State::Active | State::Blocked) => {
                    g.state = State::Paused;
                    g.note = Some("paused by you".into());
                    Ok("goal paused".into())
                }
                _ => Err("no running goal".into()),
            }),
            "resume" => {
                let text = self.update(|g| match g {
                    Some(g) if matches!(g.state, State::Paused | State::Blocked) => {
                        g.state = State::Active;
                        g.note = None;
                        g.runs = 0;
                        g.stalls = 0;
                        g.retries = 0;
                        Ok(continuation(g))
                    }
                    _ => Err("no paused or blocked goal".to_string()),
                })?;
                if let Some(h) = self.handle.get() {
                    h.remind_as("goal", text, true);
                }
                Ok("goal resumed".into())
            }
            "clear" | "stop" | "cancel" | "off" | "reset" | "none" => {
                let was = self.update(|g| g.take());
                Ok(was.map_or("no goal set".into(), |g| format!("goal cleared: {}", g.objective)))
            }
            "audit on" | "audit off" => self.update(|g| match g {
                Some(g) => {
                    g.audit = arg == "audit on";
                    Ok(format!("completion audit {}", if g.audit { "on" } else { "off" }))
                }
                None => Err("no goal".into()),
            }),
            a if a.starts_with("edit ") => {
                let (objective, criteria) = parse_goal(&a[5..]);
                if objective.is_empty() {
                    return Ok("the goal needs an objective".into());
                }
                self.update(|g| match g {
                    Some(g) => {
                        g.objective = objective;
                        g.criteria = criteria;
                        Ok("goal updated".into())
                    }
                    None => Err("no goal to edit".into()),
                })
                .or_else(Ok)
            }
            _ => Err("unknown: use /goal, /goal <objective>, pause, resume, clear, edit, audit on|off".into()),
        }
    }
}

const JUDGE_PROMPT: &str = "You check whether a coding agent has met its goal. You get the goal, its completion criteria, and the newest part of the conversation. Judge only from evidence in the conversation (commands run and their output, files changed), not from the agent's claims. Reply with one line: `MET: <why>`, `NOT_MET: <what is still missing, as guidance for the next step>`, or `IMPOSSIBLE: <why it can never be met as stated>`. Use IMPOSSIBLE only when no further work could meet it.";

fn verdict(text: &str) -> Option<Verdict> {
    let line = text
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())?
        .trim_matches('*')
        .trim();
    let (k, why) = line.split_once(':')?;
    let why = why.trim().to_string();
    match k
        .trim()
        .trim_matches('*')
        .trim()
        .to_ascii_uppercase()
        .replace(' ', "_")
        .as_str()
    {
        "MET" => Some(Verdict::Met(why)),
        "NOT_MET" => Some(Verdict::NotMet(why)),
        "IMPOSSIBLE" => Some(Verdict::Impossible(why)),
        _ => None,
    }
}

/// One line per message for the judge: prompts, replies, tool calls and the head of results.
fn render(m: &Message) -> String {
    let cut = |s: &str, n: usize| -> String {
        let c: String = s.chars().take(n).collect();
        if c.len() < s.len() { format!("{c}…") } else { c }
    };
    match m {
        Message::User { content } => {
            let t: Vec<&str> = content
                .iter()
                .filter_map(|b| match b {
                    Block::Text { text, .. } if !text.starts_with("<system-reminder>") => Some(text.as_str()),
                    _ => None,
                })
                .collect();
            if t.is_empty() {
                String::new()
            } else {
                format!("USER: {}", cut(&t.join("\n"), 2000))
            }
        }
        Message::Assistant(a) => {
            let mut s = Vec::new();
            let text = a.text();
            if !text.trim().is_empty() {
                s.push(format!("AGENT: {}", cut(&text, 2000)));
            }
            for c in a.tool_calls() {
                s.push(format!("CALL {}: {}", c.name, cut(&c.arguments.to_string(), 300)));
            }
            s.join("\n")
        }
        Message::ToolResult {
            tool_name,
            content,
            is_error,
            ..
        } => {
            let t: Vec<&str> = content
                .iter()
                .filter_map(|b| match b {
                    Block::Text { text, .. } => Some(text.as_str()),
                    _ => None,
                })
                .collect();
            let mark = if *is_error { " (error)" } else { "" };
            format!("RESULT {tool_name}{mark}: {}", cut(&t.join("\n"), 1500))
        }
        _ => String::new(),
    }
}

fn tokens(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1e6)
    } else {
        format!("{}k", n / 1000)
    }
}

/// The objective, then `Done when:` and one `- criterion` per line.
fn goal_text(g: &Goal) -> String {
    let mut s = g.objective.clone();
    if !g.criteria.is_empty() {
        s.push_str("\nDone when:");
        for c in &g.criteria {
            s.push_str(&format!("\n- {c}"));
        }
    }
    s
}

/// The inverse of [`goal_text`].
fn parse_goal(s: &str) -> (String, Vec<String>) {
    match s.split_once("Done when:") {
        Some((o, c)) => (
            o.trim().to_string(),
            c.lines()
                .map(|l| l.trim().trim_start_matches(['-', '*', '·']).trim())
                .filter(|l| !l.is_empty())
                .map(String::from)
                .collect(),
        ),
        None => (s.trim().to_string(), vec![]),
    }
}

fn continuation(g: &Goal) -> String {
    let step = if g.ordered {
        "Do the next pending step of the plan, in order"
    } else {
        "Continue with the next step"
    };
    let mut s = format!("Goal still open: {}\n{step}.", goal_text(g));
    if g.criteria.is_empty() {
        s.push_str(
            "\nIt has no checkable criteria yet: propose 1-5 with `ask_user` and set them with `goal` action `update`.",
        );
    }
    if let Some(r) = &g.reason {
        s.push_str(&format!("\nThe judge says still missing: {r}"));
    }
    s.push_str("\nWhen every criterion is met, call `goal` with action `complete` and your evidence. If you need the user, call `goal` with action `block` and say what you need.");
    if let Some(n) = &g.note {
        s.push_str(&format!("\nLast audit said: {n}"));
    }
    s
}

fn status(g: &Goal) -> String {
    let mut s = format!("Goal: {}\nState: {:?}", g.objective, g.state);
    if g.ordered {
        s.push_str(" (ordered)");
    }
    for c in &g.criteria {
        s.push_str(&format!("\n  · {c}"));
    }
    if let Some(n) = &g.note {
        s.push_str(&format!("\nNote: {n}"));
    }
    if let Some(r) = &g.reason {
        s.push_str(&format!("\nJudge's latest reason: {r}"));
    }
    let end = g.ended_ms.unwrap_or_else(novacode_core::session::now_ms);
    let mins = end.saturating_sub(g.started_ms) / 60_000;
    s.push_str(&format!(
        "\nRunning {mins} min · automatic runs: {} · tokens: {} · audit {}",
        g.runs,
        tokens(g.tokens),
        if g.audit { "on" } else { "off" }
    ));
    s.push_str("\n\n/goal pause · resume · edit · clear · audit on|off");
    s
}

/// The hidden note after `/goal <text>`; it follows the user's message.
pub const START_NOTE: &str = "The user set the goal above with /goal; it is active and shown in the panel. First draft 1-5 completion criteria that can be checked in the workspace (a test passes, a command prints X, a file has Y) and ask the user to approve them with `ask_user`, the draft as the recommended choice's preview (the user can edit it with Ctrl+G). On approval call `goal` with action `update` and the approved criteria (and a sharper objective if the user agreed). Put the steps in `todo`, then work toward it; you are woken after each turn until it is met.";

#[derive(Deserialize)]
#[serde(rename_all = "lowercase", tag = "action")]
enum Args {
    /// Start a goal (replaces the current one).
    Set {
        objective: String,
        /// Checkable completion criteria.
        criteria: Vec<String>,
        /// The todo steps must be done in order, one per run.
        #[serde(default)]
        ordered: bool,
    },
    /// Change the open goal's objective or criteria (after the user approved them).
    Update {
        #[serde(default)]
        objective: Option<String>,
        #[serde(default)]
        criteria: Option<Vec<String>>,
        #[serde(default)]
        ordered: Option<bool>,
    },
    /// Every criterion is met: give the evidence (commands run and their results).
    Complete {
        evidence: String,
    },
    /// You need the user to continue.
    Block {
        reason: String,
    },
    Status,
}

pub struct GoalTool(pub Arc<Goals>);

impl Tool for GoalTool {
    fn name(&self) -> &str {
        "goal"
    }
    fn description(&self) -> &str {
        "Work toward a goal until it is done. `set` starts one (objective + checkable criteria); `update` changes the open goal's objective or criteria; while it is open you are woken after each turn to continue. `complete` with evidence when every criterion is met (an independent auditor may check it); `block` when you need the user; `status` shows it. Only the user pauses, resumes or clears a goal."
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": { "type": "string", "enum": ["set", "update", "complete", "block", "status"] },
                "objective": { "type": "string", "description": "set/update: what is to be achieved" },
                "criteria": { "type": "array", "items": { "type": "string" }, "description": "set/update: checkable completion criteria" },
                "ordered": { "type": "boolean", "description": "set/update: the todo steps must be done in order, one per run" },
                "evidence": { "type": "string", "description": "complete: commands run and their results, per criterion" },
                "reason": { "type": "string", "description": "block: what you need from the user" }
            },
            "required": ["action"]
        })
    }
    fn sequential(&self) -> bool {
        true
    }
    fn guidelines(&self) -> &[&str] {
        &[
            "Start a `goal` when the user asks for one (/goal), or propose one yourself when a request has no clear, checkable desired result (\"refine it until it works\", \"make it better\", \"keep going\"). To propose: draft an objective and 1-5 criteria that can be checked in the workspace, then ask with `ask_user`, the draft as the recommended choice's preview (the user can edit it with Ctrl+G); call `goal` set only after approval, with the approved or edited text. A request with a clear result (a named bug fixed, a test passing, a file changed) gets no goal. A subagent never proposes one.",
            "Never call `goal` complete without having run the checks the criteria name.",
        ]
    }
    fn execute<'a>(&'a self, args: Value, _ctx: &'a ToolCtx) -> BoxFuture<'a, ToolOutput> {
        Box::pin(async move {
            let a: Args = match novacode_tools::parse(args) {
                Ok(a) => a,
                Err(e) => return e,
            };
            let g = &self.0;
            match a {
                Args::Set {
                    objective,
                    criteria,
                    ordered,
                } => {
                    let audit = g.cfg.audit && g.sup.is_some();
                    g.recent.lock().unwrap().clear();
                    g.update(|cur| *cur = Some(Goal::new(objective, criteria, ordered, audit)));
                    ToolOutput::text(format!(
                        "goal set (audit {}). Work on it now; you'll be woken to continue after each turn.",
                        if audit { "on" } else { "off" }
                    ))
                }
                Args::Update {
                    objective,
                    criteria,
                    ordered,
                } => {
                    let ok = g.update(|cur| {
                        match cur.as_mut().filter(|x| !matches!(x.state, State::Done | State::Failed)) {
                            Some(x) => {
                                if let Some(o) = objective.filter(|o| !o.trim().is_empty()) {
                                    x.objective = o;
                                }
                                if let Some(c) = criteria {
                                    x.criteria = c;
                                }
                                if let Some(o) = ordered {
                                    x.ordered = o;
                                }
                                true
                            }
                            None => false,
                        }
                    });
                    if ok {
                        ToolOutput::text("goal updated")
                    } else {
                        ToolOutput::error("no open goal")
                    }
                }
                Args::Block { reason } => {
                    let ok = g.update(|cur| match cur {
                        Some(x) if x.state == State::Active => {
                            x.state = State::Blocked;
                            x.note = Some(reason.clone());
                            true
                        }
                        _ => false,
                    });
                    if !ok {
                        return ToolOutput::error("no active goal");
                    }
                    g.notice(format!("goal blocked: {reason} (/goal resume when it's sorted)"));
                    ToolOutput::text("goal blocked; the user has been told. Stop here and explain what you need.")
                }
                Args::Status => ToolOutput::text(g.current().map_or("no goal".into(), |x| status(&x))),
                Args::Complete { evidence } => {
                    let Some(goal) = g.current().filter(|x| x.state == State::Active) else {
                        return ToolOutput::error("no active goal");
                    };
                    match g.audit(&goal, &evidence).await {
                        Ok(note) => {
                            let approved = note.is_some();
                            g.finish(note);
                            ToolOutput::text(if approved {
                                "goal done: the auditor approved it."
                            } else {
                                "goal done."
                            })
                        }
                        Err(feedback) => {
                            g.update(|cur| {
                                if let Some(x) = cur {
                                    x.state = State::Active;
                                    x.note = Some(feedback.clone());
                                }
                            });
                            ToolOutput::error(feedback)
                        }
                    }
                }
            }
        })
    }
}

fn approved(report: &str) -> bool {
    report.lines().find(|l| !l.trim().is_empty()).is_some_and(|l| {
        l.trim()
            .trim_matches('*')
            .trim()
            .eq_ignore_ascii_case("VERDICT: approved")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn goals(dir: &std::path::Path, max_tokens: Option<u64>) -> Arc<Goals> {
        Goals::new(
            dir.join("g.json"),
            None,
            Config {
                max_runs: 3,
                max_tokens,
                audit: true,
                audit_model: None,
                judge: None,
            },
        )
    }

    fn tmp(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("nc-goal-{tag}-{}", std::process::id()))
    }

    #[tokio::test]
    async fn loop_continues_stalls_and_caps() {
        let d = tmp("loop");
        let g = goals(&d, None);
        g.start("ship it".into());
        g.command("edit ship it\nDone when:\n- tests pass").unwrap();
        let t = g.after_run(2, Some(StopReason::Stop)).await.unwrap();
        assert!(t.contains("ship it") && t.contains("- tests pass"), "{t}");
        assert!(g.after_run(0, None).await.is_some(), "one quiet run is fine");
        assert!(g.after_run(0, None).await.is_none(), "two in a row: stalled");
        assert_eq!(g.current().unwrap().state, State::Paused);
        // State survives a restart; resume resets the counters.
        let g2 = goals(&d, None);
        assert_eq!(g2.current().unwrap().state, State::Paused);
        g2.command("resume").unwrap();
        for _ in 0..3 {
            assert!(g2.after_run(1, None).await.is_some());
        }
        assert!(g2.after_run(1, None).await.is_none(), "goal.maxRuns");
        assert!(g2.current().unwrap().note.unwrap().contains("maxRuns"));
        assert_eq!(g2.command("cancel").unwrap(), "goal cleared: ship it");
        assert!(goals(&d, None).current().is_none());
        assert_eq!(g2.command("stop").unwrap(), "no goal set");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[tokio::test]
    async fn budget_and_errors_pause() {
        let d = tmp("budget");
        let g = goals(&d, Some(1000));
        g.start("big job".into());
        g.update(|x| x.as_mut().unwrap().tokens = 1200);
        let t = g.after_run(1, None).await.unwrap();
        assert!(t.contains("token budget") && t.contains("Summarise"), "{t}");
        assert_eq!(g.current().unwrap().state, State::Paused);
        g.command("resume").unwrap();
        g.update(|x| x.as_mut().unwrap().tokens = 0);
        let auth = StopReason::Error {
            kind: novacode_types::ErrorKind::Auth,
        };
        assert!(g.after_run(1, Some(auth)).await.is_none());
        assert!(g.current().unwrap().note.unwrap().contains("Auth"));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn edit_text_round_trips_and_verdicts_parse() {
        let d = tmp("edit");
        let g = goals(&d, None);
        g.start("make it fast".into());
        g.command("edit make it faster\nDone when:\n- p95 < 120 ms\n- tests pass")
            .unwrap();
        let e = g.edit_text().unwrap();
        assert_eq!(e, "/goal edit make it faster\nDone when:\n- p95 < 120 ms\n- tests pass");
        assert_eq!(g.current().unwrap().criteria, ["p95 < 120 ms", "tests pass"]);
        assert_eq!(
            verdict("NOT_MET: run the tests"),
            Some(Verdict::NotMet("run the tests".into()))
        );
        assert_eq!(verdict("**MET**: all green"), Some(Verdict::Met("all green".into())));
        assert_eq!(
            verdict("Impossible: no such API"),
            Some(Verdict::Impossible("no such API".into()))
        );
        assert_eq!(verdict("I think so"), None);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn verdict_line() {
        assert!(approved("\nVERDICT: approved\n- ok"));
        assert!(approved("**VERDICT: approved**"));
        assert!(!approved("VERDICT: rejected\nVERDICT: approved"));
    }
}
