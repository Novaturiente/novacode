//! Task links (L10.9 "Relay of questions and approvals", "Reports and the inbox",
//! "The super agent"): super's code keeps one socket connection per live task.
//!
//! - **Relay:** the task's `ask_user` forms and approvals are re-opened in super's
//!   windows with the label in front ([`crate::relay`]), keyed by (task, card id)
//!   so a reconnect opens no duplicate; a card answered in the task's own window
//!   closes super's copy, also when it was answered while the link was down.
//! - **Code-made reports:** [`Watcher`] turns the task's turn ends into board events:
//!   `blocked` for a turn on super's prompts that ended without a terminal report,
//!   on an error, the length limit or a provider limit, or a goal that ended
//!   unreported; `failed` when the process exits in a live state.
//! - **Inbox:** reports from `<super dir>/inbox` are applied every second.
//! - **Wakes:** terminal and code-made reports and merge results start a turn of
//!   super's; answered cards and catch-up notes ride along with the next wake.
//! - **Provider holds:** a quota or rate limit holds the provider; when it clears,
//!   code resends to the tasks it blocked.

use super::board::{self, Event, Limit, LimitKind, MergeResult, ReportState, State};
use super::task::Super;
use crate::relay::{Card, Label};
use crate::wire::{ClientCmd, Request, ServerMsg, line};
use novacode_core::policy::Approver;
use novacode_tools::ask::Asker;
use novacode_types::{AgentEvent, ErrorKind, Message, StopReason};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};

/// A connection to a task's socket.
pub struct Conn {
    pub read: Box<dyn AsyncRead + Send + Unpin>,
    pub write: Box<dyn AsyncWrite + Send + Unpin>,
}

/// How long a provider stays on hold when its limit carried no reset time.
const HOLD_WITHOUT_RESET: u64 = 15 * 60;
/// What code sends to a task when its provider limit cleared.
pub const LIMIT_CLEARED: &str = "continue: the provider limit has cleared";

/// What an interrupted task the user chose to resume after super's restart is sent.
const RESUMED: &str = "continue: super restarted while your process was gone. First check your folder and git (what \
is committed, what already ran) and don't repeat anything outside it that already happened (a commit, a push, a \
message); your proof log is empty, so re-run your checks before you cite them.";
/// The request id super's prompts carry (`Launcher::send`): a run that consumed
/// only these is a turn on super's prompt.
pub(super) const SUPER_ID: &str = "super";
/// The request id of super's questions (`task send` with `question: true`): their
/// turns need no report; the reply wakes super.
pub(super) const SUPER_QUESTION_ID: &str = "super-question";
/// What a question prompt starts with; the catch-up after a reconnect tells it by this.
pub(super) const QUESTION_PREFIX: &str = "Super asks a question (answer in text in this turn; change nothing, \
commit nothing and don't call `report`: no report is needed):\n\n";

/// The prompt a question sends.
pub(super) fn question_prompt(message: &str) -> String {
    format!("{QUESTION_PREFIX}{message}")
}

/// What super's code tells super and the user.
pub trait Inform: Send + Sync {
    /// Start a turn of super's with `text`.
    fn wake(&self, text: String);
    /// One line for super's next wake; starts no turn.
    fn note(&self, text: String);
    /// A notice in the user's windows only.
    fn notice(&self, text: String);
    /// A desktop notification.
    fn desktop(&self, title: String, body: String);
    /// Super's session is ending: a card copy that closes now got no answer from the user.
    fn closing(&self) -> bool {
        false
    }
}

/// How much of a task's last message a code-made report carries.
const LAST_TEXT_CHARS: usize = 600;

/// Who started a run, from `agent_end`'s consumed prompts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Run {
    /// Only super's prompts, at least one of them work.
    Super,
    /// Only super's questions.
    Question,
    /// A prompt typed in a task window (no id) or sent by another client.
    User,
    /// No wire prompt: a subagent wake, a scheduled prompt, a goal reminder.
    Internal,
}

fn run_of(prompts: &[Option<String>]) -> Run {
    if prompts.is_empty() {
        Run::Internal
    } else if prompts.iter().all(|p| p.as_deref() == Some(SUPER_QUESTION_ID)) {
        Run::Question
    } else if prompts
        .iter()
        .all(|p| matches!(p.as_deref(), Some(SUPER_ID | SUPER_QUESTION_ID)))
    {
        Run::Super
    } else {
        Run::User
    }
}

/// Whether a tool result is a terminal `report` (done, blocked, failed, a verdict, a merge).
fn terminal_report(m: &Message) -> bool {
    let Message::ToolResult {
        tool_name,
        is_error: false,
        details: Some(d),
        ..
    } = m
    else {
        return false;
    };
    let r = &d["report"];
    tool_name == "report"
        && (matches!(r["state"].as_str(), Some("done" | "blocked" | "failed"))
            || r["verdict"].is_string()
            || r["merge"].is_string())
}

fn user_text(m: &Message) -> Option<String> {
    match m {
        Message::User { content } => Some(
            content
                .iter()
                .filter_map(|b| match b {
                    novacode_types::Block::Text { text, .. } => Some(text.as_str()),
                    _ => None,
                })
                .collect(),
        ),
        _ => None,
    }
}

/// The task's goal, as its goal file shows it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Goal {
    None,
    /// Active or being audited: the task keeps itself going.
    Active,
    Paused,
    Done,
    Blocked,
    Failed,
}

impl Goal {
    /// `<home>/goals/<session>.json`.
    pub fn read(home: &std::path::Path, session: &str) -> Goal {
        #[derive(serde::Deserialize)]
        struct G {
            state: crate::goals::State,
        }
        let path = home.join("goals").join(format!("{session}.json"));
        use crate::goals::State as S;
        match novacode_base::storage::read_json::<G>(&path).ok().flatten() {
            None => Goal::None,
            Some(g) => match g.state {
                S::Active | S::Auditing => Goal::Active,
                S::Paused => Goal::Paused,
                S::Done => Goal::Done,
                S::Blocked => Goal::Blocked,
                S::Failed => Goal::Failed,
            },
        }
    }
}

/// Turns one task's event stream into code-made reports. Pure: fed events, the
/// goal state and the background work it sees; returns the board event to apply.
#[derive(Debug, Default)]
pub struct Watcher {
    /// A terminal report in the current run.
    run_reported: bool,
    /// A terminal report since the last run on super's prompt ended (that run's own included).
    reported_since_super: bool,
    /// A turn on super's prompt ended unreported with background work running:
    /// checked again at the next turn end with none.
    armed: bool,
    processes: HashSet<String>,
    /// Subagent id → still running.
    subagents: HashMap<String, bool>,
    last_error: Option<String>,
    /// The current run's last assistant text: a code-made report carries it.
    last_text: Option<String>,
    retry_after_ms: Option<u64>,
    /// A question run ended: its reply for super.
    reply: Option<String>,
    goal: Option<Goal>,
    /// A run is going: a goal that ends meanwhile is judged at its end (R18).
    running: bool,
    /// The catch-up note already given: (super's prompt hash, messages after it).
    noted: Option<(String, usize)>,
    /// The provider of the task's model (`provider/model`), for holds.
    pub provider: String,
}

impl Watcher {
    pub fn new(provider: &str) -> Watcher {
        Watcher {
            provider: provider.to_string(),
            ..Watcher::default()
        }
    }

    fn background(&self) -> bool {
        !self.processes.is_empty() || self.subagents.values().any(|r| *r)
    }

    /// The current run's last assistant text, cut to about [`LAST_TEXT_CHARS`].
    pub fn last_text(&self) -> Option<String> {
        let t = self.last_text.as_deref()?.trim();
        Some(match t.char_indices().nth(LAST_TEXT_CHARS) {
            Some((i, _)) => format!("{}…", &t[..i]),
            None => t.to_string(),
        })
    }

    /// The reply of the question run that just ended, once.
    pub fn take_reply(&mut self) -> Option<String> {
        self.reply.take()
    }

    /// The session state (hello, state updates): its running processes.
    pub fn state(&mut self, s: &crate::wire::State) {
        self.processes = s.processes.iter().cloned().collect();
    }

    /// One event of the task. `goal` is read at turn ends.
    pub fn event(&mut self, ev: &AgentEvent, goal: impl FnOnce() -> Goal) -> Option<Event> {
        match ev {
            AgentEvent::AgentStart => {
                self.running = true;
                self.run_reported = false;
                self.last_error = None;
                self.last_text = None;
                self.retry_after_ms = None;
            }
            AgentEvent::Message(m) => {
                if terminal_report(m) {
                    self.run_reported = true;
                    self.reported_since_super = true;
                    self.armed = false;
                }
                if let Message::Assistant(a) = m {
                    if let Some(e) = &a.error_message {
                        self.last_error = Some(e.clone());
                    }
                    let text = a.text();
                    if !text.trim().is_empty() {
                        self.last_text = Some(text);
                    }
                }
            }
            AgentEvent::Retry {
                error: ErrorKind::RateLimit { retry_after_ms },
                ..
            } => self.retry_after_ms = *retry_after_ms,
            AgentEvent::ProcessStarted { id, .. } => {
                self.processes.insert(id.clone());
            }
            AgentEvent::ProcessEnded { id, .. } => {
                self.processes.remove(id);
            }
            AgentEvent::Ui {
                ext,
                op: novacode_types::ui::UiOp::Publish { node, .. },
            } if ext == "agents" => {
                self.subagents.insert(node.id.clone(), !node.dim);
            }
            AgentEvent::AgentEnd { stop, prompts } => {
                self.running = false;
                let goal = goal();
                let ev = self.end(run_of(prompts), stop, goal);
                // A goal that ended in this run, judged now its reports are in.
                let ended = self.goal(goal);
                return match ev {
                    Some(Event::TurnError(_) | Event::TurnLimit(_)) => ev,
                    _ => ended.or(ev),
                };
            }
            _ => {}
        }
        None
    }

    fn end(&mut self, run: Run, stop: &StopReason, goal: Goal) -> Option<Event> {
        match run {
            Run::User => {
                self.armed = false;
                None
            }
            Run::Internal => {
                let due = self.armed && !self.background() && goal != Goal::Active;
                if due {
                    self.armed = false;
                    return Some(Event::TurnUnreported);
                }
                None
            }
            Run::Super => {
                self.reported_since_super = self.run_reported;
                self.armed = false;
                if self.run_reported {
                    return None;
                }
                self.turn_end(stop, goal)
            }
            // Errors and limits block as on super's prompt; otherwise the reply wakes super.
            Run::Question => {
                let failed = self.failure(stop);
                if failed.is_none() && !matches!(stop, StopReason::Aborted) {
                    self.reply = Some(self.last_text().unwrap_or_else(|| "(no text)".into()));
                }
                failed
            }
        }
    }

    /// A turn on super's prompt ended with no terminal report.
    fn turn_end(&mut self, stop: &StopReason, goal: Goal) -> Option<Event> {
        if let Some(ev) = self.failure(stop) {
            return Some(ev);
        }
        match stop {
            // Stopped by super (its state is no longer live) or in the task's window.
            StopReason::Aborted => None,
            _ if goal == Goal::Active => None,
            _ if self.background() => {
                self.armed = true;
                None
            }
            _ => Some(Event::TurnUnreported),
        }
    }

    /// A turn that ended on an error, the length limit or a provider limit.
    fn failure(&self, stop: &StopReason) -> Option<Event> {
        let limit = |kind, reset_at| {
            Some(Event::TurnLimit(Limit {
                kind,
                provider: self.provider.clone(),
                reset_at,
            }))
        };
        match stop {
            StopReason::Error {
                kind: ErrorKind::QuotaExhausted,
            } => limit(LimitKind::Quota, None),
            StopReason::Error {
                kind: ErrorKind::RateLimit { retry_after_ms },
            } => {
                let ms = retry_after_ms.or(self.retry_after_ms);
                limit(LimitKind::RateLimit, ms.map(|ms| board::now() + ms.div_ceil(1000)))
            }
            StopReason::Error { .. } => Some(Event::TurnError(
                self.last_error
                    .clone()
                    .unwrap_or_else(|| "the model call failed".into()),
            )),
            StopReason::Length => Some(Event::TurnError("the reply hit the output length limit".into())),
            _ => None,
        }
    }

    /// The goal file now says `now` (ignored mid-run: the run's end judges it): a goal
    /// that left the active state as done,
    /// blocked or failed with no terminal report since super's last prompt is reported.
    pub fn goal(&mut self, now: Goal) -> Option<Event> {
        if self.running {
            return None;
        }
        let before = self.goal.replace(now);
        if before != Some(Goal::Active) || self.reported_since_super {
            return None;
        }
        let how = match now {
            Goal::Done => "goal done without a report",
            Goal::Blocked => "goal blocked",
            Goal::Failed => "goal can't be met",
            Goal::None | Goal::Active | Goal::Paused => return None,
        };
        Some(Event::GoalEnded(how.into()))
    }

    /// After a reconnect: the turn super's last prompt (hash `last_prompt`) started
    /// may have ended unseen. Reported only when nothing but super's prompt came in.
    pub fn catch_up(&mut self, history: &[Message], last_prompt: Option<&str>, busy: bool, goal: Goal) -> CatchUp {
        // A run end missed while disconnected must not hold goal ends back.
        self.running = busy;
        let Some(hash) = last_prompt else {
            return CatchUp::Nothing;
        };
        if busy {
            return CatchUp::Nothing;
        }
        let Some(i) = history
            .iter()
            .rposition(|m| user_text(m).is_some_and(|t| board::prompt_hash(&t) == hash))
        else {
            return CatchUp::Nothing;
        };
        let after = &history[i + 1..];
        if after.iter().any(|m| user_text(m).is_some()) {
            let key = (hash.to_string(), after.len());
            if self.noted.as_ref() == Some(&key) {
                return CatchUp::Nothing;
            }
            self.noted = Some(key);
            return CatchUp::Note;
        }
        if after.iter().any(terminal_report) {
            return CatchUp::Nothing;
        }
        let Some(last) = after.iter().rev().find_map(|m| match m {
            Message::Assistant(a) => Some(a),
            _ => None,
        }) else {
            return CatchUp::Nothing;
        };
        self.run_reported = false;
        self.last_error = last.error_message.clone();
        self.last_text = Some(last.text()).filter(|t| !t.trim().is_empty());
        let run = match user_text(&history[i]) {
            Some(t) if t.starts_with(QUESTION_PREFIX) => Run::Question,
            _ => Run::Super,
        };
        match self.end(run, &last.stop, goal) {
            Some(ev) => CatchUp::Report(ev),
            None => CatchUp::Nothing,
        }
    }
}

#[derive(Debug, PartialEq)]
pub enum CatchUp {
    Nothing,
    Report(Event),
    /// Can't tell whose turn it was: a one-line note for super.
    Note,
}

/// The links of all live tasks, plus the inbox and the provider holds.
pub struct Links {
    sup: Weak<Super>,
    asker: Arc<dyn Asker>,
    approver: Arc<dyn Approver>,
    inform: Arc<dyn Inform>,
    me: Weak<Links>,
    inner: Mutex<Inner>,
    kick: Arc<tokio::sync::Notify>,
}

#[derive(Default)]
struct Inner {
    /// Task id → its link.
    links: HashMap<String, LinkSlot>,
    /// (task id, the task's card id) → super's copy.
    cards: HashMap<(String, u64), tokio::task::AbortHandle>,
    tasks: Vec<tokio::task::AbortHandle>,
}

struct LinkSlot {
    session: String,
    writer: Arc<tokio::sync::Mutex<Option<Box<dyn AsyncWrite + Send + Unpin>>>>,
    task: tokio::task::AbortHandle,
}

impl Drop for Links {
    fn drop(&mut self) {
        let g = self.inner.get_mut().unwrap();
        for l in g.links.values() {
            l.task.abort();
        }
        for c in g.cards.values() {
            c.abort();
        }
        for t in &g.tasks {
            t.abort();
        }
    }
}

impl Links {
    /// Links for `sup`'s tasks; starts the inbox and hold loop and links the live tasks.
    pub fn start(
        sup: &Arc<Super>,
        asker: Arc<dyn Asker>,
        approver: Arc<dyn Approver>,
        inform: Arc<dyn Inform>,
    ) -> Arc<Links> {
        let links = Arc::new_cyclic(|me| Links {
            sup: Arc::downgrade(sup),
            asker,
            approver,
            inform,
            me: me.clone(),
            inner: Mutex::default(),
            kick: Arc::default(),
        });
        let _ = sup.links.set(links.clone());
        links.recover(sup);
        let (me, kick) = (Arc::downgrade(&links), links.kick.clone());
        let t = tokio::spawn(async move {
            loop {
                let Some(l) = me.upgrade() else { break };
                l.tick().await;
                drop(l);
                let _ = tokio::time::timeout(Duration::from_secs(1), kick.notified()).await;
            }
        });
        links.inner.lock().unwrap().tasks.push(t.abort_handle());
        links
    }

    /// Restart recovery, once at start: the board's tasks whose process is gone. A dead
    /// review is closed and a new one started for its task; dead tasks in `running` or
    /// `waiting-you` become `interrupted`, a dead merge follows the merge exits; the user
    /// is asked once which interrupted tasks to resume. Live tasks are linked by `tick`.
    fn recover(&self, sup: &Arc<Super>) {
        let dead: Vec<board::Task> = {
            let b = sup.board.lock().unwrap();
            b.tasks
                .iter()
                .filter(|t| t.state != State::Archived)
                .filter(|t| t.session.as_deref().is_some_and(|s| !sup.launcher.alive(s)))
                .cloned()
                .collect()
        };
        let mut interrupted = Vec::new();
        for t in &dead {
            match t.kind {
                board::Kind::Review if matches!(t.state, State::Running | State::WaitingYou) => {
                    self.restart_review(sup, t)
                }
                board::Kind::Review => {}
                board::Kind::Work => {
                    let Ok(Ok(n)) = sup.edit(|b| b.apply(&t.id, Event::Restart)) else {
                        continue;
                    };
                    if let Some(why) = sup.merge_exit(&t.id, &n) {
                        self.inform.note(format!(
                            "{} was merging when super went down; it is blocked: {why}",
                            t.label
                        ));
                    }
                    if sup.row(&t.id).is_ok_and(|r| r.state == State::Interrupted) && n.from != n.to {
                        interrupted.push(t.label.clone());
                    }
                }
            }
        }
        if interrupted.is_empty() {
            return;
        }
        let (sup, asker, me) = (sup.clone(), self.asker.clone(), self.me.clone());
        let t = tokio::spawn(async move {
            let q = novacode_types::Question {
                question: "Super restarted and found these tasks' processes gone; they are interrupted. Which should \
                           resume? Each first checks its folder and git before repeating anything outside it."
                    .into(),
                header: Some("Resume tasks".into()),
                options: interrupted
                    .iter()
                    .map(|l| novacode_types::Choice {
                        label: l.clone(),
                        description: None,
                        preview: None,
                        recommended: false,
                    })
                    .collect(),
                multi: true,
            };
            let picked = asker
                .ask(vec![q])
                .await
                .and_then(|r| r.into_iter().next())
                .map(|r| r.chosen)
                .unwrap_or_default();
            let mut lines = Vec::new();
            for label in &interrupted {
                if !picked.contains(label) {
                    continue;
                }
                let send = serde_json::json!({"action": "send", "task": label, "message": RESUMED});
                match sup.run(send).await {
                    Ok(_) => lines.push(format!("{label} resumed")),
                    Err(e) => lines.push(format!("{label} didn't resume: {e}")),
                }
            }
            let rest: Vec<&str> = interrupted
                .iter()
                .filter(|l| !picked.contains(l))
                .map(String::as_str)
                .collect();
            if !rest.is_empty() {
                lines.push(format!("{} stay interrupted", rest.join(", ")));
            }
            if let Some(me) = me.upgrade() {
                me.inform
                    .note(format!("After super's restart the user chose: {}.", lines.join("; ")));
            }
        });
        self.inner.lock().unwrap().tasks.push(t.abort_handle());
    }

    /// A review task found dead at restart: closed, and a new review for its task (no round used).
    fn restart_review(&self, sup: &Super, rv: &board::Task) {
        let of = rv.reviewer_of.clone().unwrap_or_default();
        sup.retire_review(&rv.id);
        let current = sup
            .row(&of)
            .is_ok_and(|t| t.state == State::Reviewing && t.reviewer.as_deref() == Some(rv.id.as_str()));
        if !current {
            return;
        }
        let _ = sup.edit(|b| {
            if let Some(t) = b.tasks.iter_mut().find(|t| t.id == of) {
                t.reviewer = None;
            }
        });
        match sup.open_review(&of, None) {
            Ok(new) => self.inform.note(format!(
                "{of}'s review task {} was gone after super's restart; its new review task is {new}.",
                rv.label
            )),
            Err(e) => self.inform.wake(format!(
                "{of} is blocked: its review task {} was gone after super's restart and a new one can't start: {e}",
                rv.label
            )),
        }
    }

    /// Check the inbox and the holds now (tests; a report just written).
    pub fn kick(&self) {
        self.kick.notify_one();
    }

    /// Start a turn of super's (code outside the links: a review that can't start).
    pub(super) fn wake(&self, text: String) {
        self.inform.wake(text);
    }

    /// One pass: inbox, holds, links of live tasks not linked yet, then the queue.
    async fn tick(&self) {
        let Some(sup) = self.sup.upgrade() else { return };
        self.apply_inbox(&sup).await;
        self.branch_moves(&sup);
        self.lift_holds(&sup).await;
        let rows: Vec<(String, String)> = {
            let b = sup.board.lock().unwrap();
            b.tasks
                .iter()
                .filter(|t| !matches!(t.state, State::Archived | State::Queued))
                .filter_map(|t| Some((t.id.clone(), t.session.clone()?)))
                .collect()
        };
        for (id, session) in rows {
            let linked = self.inner.lock().unwrap().links.contains_key(&id);
            // A dead one found here was handled by restart recovery (`recover`), not an exit seen live.
            if !linked && sup.launcher.alive(&session) {
                self.open(&id, &session);
            }
        }
        // Reports, exits and lifted holds free slots and providers.
        for n in sup.pump().await {
            self.inform.note(n);
        }
    }

    /// Link `id`'s session (a task just started, or found live). A link to an older
    /// session of the same task is replaced.
    pub fn open(&self, id: &str, session: &str) {
        let mut g = self.inner.lock().unwrap();
        if g.links.get(id).is_some_and(|l| l.session == session) {
            return;
        }
        if let Some(old) = g.links.remove(id) {
            old.task.abort();
        }
        let writer = Arc::new(tokio::sync::Mutex::new(None));
        let (me, id2, s2, w2) = (self.me.clone(), id.to_string(), session.to_string(), writer.clone());
        let task = tokio::spawn(async move {
            run_link(me, id2, s2, w2).await;
        });
        g.links.insert(
            id.to_string(),
            LinkSlot {
                session: session.to_string(),
                writer,
                task: task.abort_handle(),
            },
        );
    }

    /// Apply `ev` to task `id`: save the board, wake or note, notify. Refused events
    /// (no row for this pair in the spec's table) are logged and ignored. Returns
    /// whether a running slot may have freed.
    fn apply(&self, sup: &Super, id: &str, ev: Event) -> bool {
        self.apply_with(sup, id, ev, "")
    }

    /// [`apply`](Self::apply), with `extra` lines for the wake (a report's checks).
    fn apply_with(&self, sup: &Super, id: &str, ev: Event, extra: &str) -> bool {
        let Ok(before) = sup.row(id) else { return false };
        if before.kind == board::Kind::Review {
            // A review row only runs or waits; its turn ends and exits are crashes of the review.
            return match crash_reason(&ev) {
                Some(why) => self.review_crashed(sup, &before, why),
                None => self.apply_row(sup, id, ev),
            };
        }
        let had_hold = match &ev {
            Event::TurnLimit(l) => sup.board.lock().unwrap().holds.contains_key(&l.provider),
            _ => true,
        };
        let n = match sup.edit(|b| b.apply(id, ev.clone())) {
            Ok(Ok(n)) => n,
            Ok(Err(e)) => {
                tracing::debug!(task = id, %e, "super: event ignored");
                return false;
            }
            Err(e) => {
                tracing::warn!(task = id, %e, "super: board");
                return false;
            }
        };
        let mut extra = extra.to_string();
        if n.effects.contains(&board::Effect::CheckMergeLeft) {
            extra += &match sup.merge_exit(id, &n) {
                Some(why) => format!("\nThe merge lock is released; it is blocked: {why}."),
                None => "\nThe merge lock is released; the main checkout shows no half-done merge.".into(),
            };
        }
        let Ok(t) = sup.row(id) else { return false };
        if let Event::TurnLimit(limit) = &ev {
            // No reset time known: the hold ends after a fixed wait.
            let _ = sup.edit(|b| {
                if let Some(h) = b.holds.get_mut(&limit.provider)
                    && h.reset_at.is_none()
                {
                    h.reset_at = Some(board::now() + HOLD_WITHOUT_RESET);
                }
            });
            if !had_hold {
                self.inform.notice(format!(
                    "Provider {} is on hold ({}){}: its queued tasks wait; blocked tasks are resent when it clears.",
                    limit.provider,
                    limit.kind.label(),
                    until(sup, &limit.provider)
                ));
            }
        }
        if let Some(old) = before.reviewer.as_deref().filter(|_| t.reviewer != before.reviewer) {
            sup.retire_review(old);
        }
        let t = if n.effects.iter().any(|e| matches!(e, board::Effect::StartReview { .. })) {
            match sup.open_review(id, None) {
                Ok(review) => {
                    extra += &format!("\nIts review task {review} starts.");
                }
                Err(e) => extra += &format!("\nIt is blocked now: {e}"),
            }
            match sup.row(id) {
                Ok(t) => t,
                Err(_) => return false,
            }
        } else {
            t
        };
        let changed = n.from != t.state;
        if changed && matches!(t.state, State::Ready | State::Blocked | State::Failed) {
            let why = t.block.as_ref().map(|b| b.reason.clone()).or(t.summary.clone());
            self.inform
                .desktop(format!("{} {}", t.label, t.state.name()), why.unwrap_or_default());
        }
        let wakes = match &ev {
            // Over a code-made block the task's own report always counts (R18).
            Event::Report { state, .. } => {
                *state != ReportState::Progress && (changed || before.block.as_ref().is_some_and(|b| b.code_made))
            }
            Event::MergeReport(_) | Event::Verdict(_) | Event::BranchMoved | Event::ReviewCantStart(_) => true,
            Event::ReviewCrashed(_) => t.state == State::Blocked,
            Event::TurnUnreported
            | Event::TurnError(_)
            | Event::TurnLimit(_)
            | Event::GoalEnded(_)
            | Event::ProcessExit => changed,
            _ => false,
        };
        if wakes {
            self.inform.wake(format!("{}{extra}", wake_text(&t, &ev)));
        } else if let Event::ReviewCrashed(why) = &ev {
            self.inform
                .note(format!("{}'s review task crashed ({why}); one retry.{extra}", t.label));
        }
        changed && n.from == State::Running
    }

    /// A question run of task `id` ended: its reply wakes super.
    fn replied(&self, sup: &Super, id: &str, w: &mut Watcher) {
        if let Some(r) = w.take_reply() {
            let label = sup.row(id).map(|t| t.label).unwrap_or_else(|_| id.to_string());
            self.inform.wake(format!("{label} replied: {r}"));
        }
    }

    /// A review row's own change (start, card, archive): no wake.
    fn apply_row(&self, sup: &Super, id: &str, ev: Event) -> bool {
        match sup.edit(|b| b.apply(id, ev)) {
            Ok(Ok(n)) => n.from == State::Running && n.to != State::Running,
            Ok(Err(e)) => {
                tracing::debug!(task = id, %e, "super: event ignored");
                false
            }
            Err(e) => {
                tracing::warn!(task = id, %e, "super: board");
                false
            }
        }
    }

    /// A review task died or ended its turn without a verdict: one new review, then
    /// the reviewed task is blocked with the reason.
    fn review_crashed(&self, sup: &Super, rv: &board::Task, why: String) -> bool {
        if rv.state == State::Archived {
            return false;
        }
        let why = match &rv.summary {
            Some(s) => format!("{why}; its last report: {s}"),
            None => why,
        };
        let of = rv.reviewer_of.clone().unwrap_or_default();
        let current = sup
            .row(&of)
            .is_ok_and(|t| t.reviewer.as_deref() == Some(rv.id.as_str()));
        if !current {
            sup.retire_review(&rv.id);
            return true;
        }
        self.apply_with(sup, &of, Event::ReviewCrashed(format!("{}: {why}", rv.label)), "");
        true
    }

    /// Reviewed tasks whose branch moved while their review ran: verdict discarded, back to `running`.
    fn branch_moves(&self, sup: &Super) {
        let reviewing: Vec<board::Task> = {
            let b = sup.board.lock().unwrap();
            b.tasks
                .iter()
                .filter(|t| t.state == State::Reviewing && t.reviewer.is_some())
                .cloned()
                .collect()
        };
        for t in reviewing {
            if sup.branch_moved(&t) {
                self.apply(sup, &t.id, Event::BranchMoved);
            }
        }
    }

    /// A review task's verdict: applied to the task it reviews. Code forwards the
    /// findings of `needs-proof` and `rejected` word for word; the board counts the round.
    async fn verdict(&self, sup: &Super, review: &str, f: &serde_json::Map<String, Value>) {
        let Ok(rv) = sup.row(review) else { return };
        if let Some(field) = f.get("baseline").filter(|v| v.is_object()) {
            super::review::record_baseline(sup, &rv, field);
        }
        let of = rv.reviewer_of.clone().unwrap_or_default();
        let current = sup
            .row(&of)
            .is_ok_and(|t| t.state == State::Reviewing && t.reviewer.as_deref() == Some(review));
        if !current {
            sup.retire_review(review);
            return;
        }
        // The branch moved after the commit under review: the verdict is about old work.
        if sup.row(&of).is_ok_and(|t| sup.branch_moved(&t)) {
            self.apply(sup, &of, Event::BranchMoved);
            return;
        }
        let said = f.get("verdict").and_then(Value::as_str);
        let verdict = board::Verdict::ALL
            .into_iter()
            .find(|v| Some(v.name()) == said)
            .unwrap_or(board::Verdict::Rejected);
        let findings: Vec<String> = f
            .get("findings")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|v| v.as_str().map(String::from))
            .collect();
        let list: String = findings.iter().map(|x| format!("\n- {x}")).collect();
        let extra = if list.is_empty() {
            String::new()
        } else {
            format!("\nFindings:{list}")
        };
        self.apply_with(sup, &of, board::Event::Verdict(verdict), &extra);
        let Ok(t) = sup.row(&of) else { return };
        if verdict != board::Verdict::Approved && t.state == State::Running {
            let name = verdict.name();
            let text = format!(
                "Your review ({}) says {name} about commit {}. Its findings, word for word:{list}\n\n\
Address every finding, commit, re-run your checks and report `done` again.",
                rv.label,
                t.done.as_ref().and_then(|d| d.commit.as_deref()).unwrap_or("?"),
            );
            if let Err(e) = sup.deliver(&of, text).await {
                self.inform
                    .note(format!("{}: the review findings couldn't be sent: {e}", t.label));
            }
        }
    }

    /// Apply the inbox's reports in order.
    async fn apply_inbox(&self, sup: &Super) {
        let pending = {
            let b = sup.board.lock().unwrap();
            match super::inbox::read(&sup.dir, &b) {
                Ok(p) => p,
                Err(e) => {
                    tracing::warn!(%e, "super: inbox");
                    return;
                }
            }
        };
        for p in pending {
            let r = &p.report;
            let f = &r.fields;
            let text = |k: &str| f.get(k).and_then(Value::as_str).map(str::to_string);
            let summary = text("summary").unwrap_or_default();
            let ev = match (text("state").as_deref(), text("merge").as_deref()) {
                (Some(s), _) => {
                    let state = match s {
                        "progress" => ReportState::Progress,
                        "done" => ReportState::Done,
                        "blocked" => ReportState::Blocked,
                        _ => ReportState::Failed,
                    };
                    Event::Report {
                        state,
                        summary,
                        question: text("question"),
                    }
                }
                (None, Some(m)) => Event::MergeReport(match m {
                    "merged" => MergeResult::Merged,
                    "pr-opened" => MergeResult::PrOpened,
                    _ => MergeResult::Failed(summary),
                }),
                (None, None) => {
                    if f.get("verdict").is_some_and(Value::is_string) {
                        self.verdict(sup, &r.task, f).await;
                    }
                    self.applied(sup, p);
                    continue;
                }
            };
            // The transition reads the new facts (R11 approved commit), so set them first and undo on refusal.
            let mut prior_done = None;
            if let Event::Report {
                state: ReportState::Done,
                ..
            } = &ev
            {
                let done = done_facts(f);
                let _ = sup.edit(|b| {
                    if let Some(t) = b.tasks.iter_mut().find(|t| t.id == r.task) {
                        prior_done = Some(t.done.replace(done));
                    }
                });
            }
            if f.get("write").and_then(Value::as_bool) == Some(true) {
                let _ = sup.edit(|b| {
                    if let Some(t) = b.tasks.iter_mut().find(|t| t.id == r.task) {
                        t.write = true;
                    }
                });
            }
            if !self.apply_with(sup, &r.task, ev, &checks_text(f))
                && let Some(old) = prior_done
            {
                let _ = sup.edit(|b| {
                    if let Some(t) = b.tasks.iter_mut().find(|t| t.id == r.task) {
                        t.done = old;
                    }
                });
            }
            self.applied(sup, p);
        }
    }

    /// Record a report as applied and delete its file.
    fn applied(&self, sup: &Super, p: super::inbox::Pending) {
        let (task, id) = (p.report.task.clone(), p.report.id.clone());
        let saved = sup.edit(|b| {
            if let Some(t) = b.tasks.iter_mut().find(|t| t.id == task) {
                t.applied.push(id);
            }
        });
        if saved.is_ok()
            && let Err(e) = p.done()
        {
            tracing::warn!(%e, "super: inbox file not deleted");
        }
    }

    /// Lift the holds whose time came, and resend to the tasks they blocked.
    async fn lift_holds(&self, sup: &Super) {
        let now = board::now();
        let due: Vec<String> = {
            let b = sup.board.lock().unwrap();
            b.holds
                .iter()
                .filter(|(_, l)| l.reset_at.is_some_and(|r| r <= now))
                .map(|(p, _)| p.clone())
                .collect()
        };
        for provider in due {
            let blocked: Vec<String> = sup
                .edit(|b| {
                    b.lift_hold(&provider);
                    b.tasks
                        .iter()
                        .filter(|t| blocked_on(t, &provider))
                        .map(|t| t.id.clone())
                        .collect()
                })
                .unwrap_or_default();
            for id in blocked {
                if sup.edit(|b| b.apply(&id, Event::LimitCleared)).is_ok_and(|r| r.is_ok())
                    && let Err(e) = sup.deliver(&id, LIMIT_CLEARED.into()).await
                {
                    self.inform
                        .note(format!("{id}: the resend after the provider limit failed: {e}"));
                }
            }
        }
    }

    /// The task's process is gone: `failed` from a live state, else unchanged.
    fn exited(&self, id: &str, session: &str) {
        let Some(sup) = self.sup.upgrade() else { return };
        let mut g = self.inner.lock().unwrap();
        if g.links.get(id).is_some_and(|l| l.session == session) {
            g.links.remove(id);
        }
        let keys: Vec<_> = g.cards.keys().filter(|(t, _)| t == id).cloned().collect();
        for k in keys {
            if let Some(c) = g.cards.remove(&k) {
                c.abort();
            }
        }
        drop(g);
        // A resumed task runs a new session; only its current one counts.
        if sup.row(id).ok().and_then(|t| t.session).as_deref() != Some(session) {
            return;
        }
        if self.apply(&sup, id, Event::ProcessExit) {
            self.kick();
        }
    }

    /// A card the task opened: relay it once per (task, card id).
    fn card(&self, id: &str, card: Card) {
        let Some(sup) = self.sup.upgrade() else { return };
        let key = (id.to_string(), card.id());
        let mut g = self.inner.lock().unwrap();
        if g.cards.contains_key(&key) {
            return;
        }
        let Some(writer) = g.links.get(id).map(|l| l.writer.clone()) else {
            return;
        };
        let label = sup.row(id).map(|t| t.label).unwrap_or_else(|_| id.to_string());
        let (me, id2, what) = (self.me.clone(), id.to_string(), card.what());
        let (asker, approver) = (self.asker.clone(), self.approver.clone());
        let card_id = card.id();
        let t = tokio::spawn(async move {
            let l = format!("[{label}] ");
            let (cmd, said) =
                crate::relay::relay(asker.as_ref(), approver.as_ref(), card, Label { ask: &l, approval: &l }).await;
            // Super shutting down closed its copy: that is no answer; the card stays open in the task.
            if me.upgrade().is_none_or(|me| me.inform.closing()) {
                return;
            }
            if let Some(w) = writer.lock().await.as_mut() {
                let req = Request { id: None, cmd };
                let _ = w.write_all(line(&req).as_bytes()).await;
                let _ = w.flush().await;
            }
            if let Some(me) = me.upgrade() {
                me.inform.note(format!("{label} asked {what}; you answered: {said}"));
                me.card_closed(&id2, card_id);
            }
        });
        g.cards.insert(key, t.abort_handle());
        drop(g);
        self.apply(&sup, id, Event::CardOpened);
        let label = sup.row(id).map(|t| t.label).unwrap_or_default();
        self.inform
            .desktop(format!("{label} asks"), "its question waits in super's window".into());
    }

    /// The task's card was answered in its own window: close super's copy.
    fn card_done(&self, id: &str, card_id: u64) {
        let Some(h) = self.inner.lock().unwrap().cards.remove(&(id.to_string(), card_id)) else {
            return;
        };
        h.abort();
        if let Some(sup) = self.sup.upgrade() {
            let label = sup.row(id).map(|t| t.label).unwrap_or_default();
            self.inform
                .note(format!("{label}'s question was answered in its own window"));
        }
        self.card_closed(id, card_id);
    }

    /// After a reconnect's catch-up: close super's copies of the task's cards it no longer has open.
    fn cards_gone(&self, id: &str, open: &HashSet<u64>) {
        let gone: Vec<u64> = {
            let g = self.inner.lock().unwrap();
            g.cards
                .keys()
                .filter(|(t, c)| t == id && !open.contains(c))
                .map(|(_, c)| *c)
                .collect()
        };
        for c in gone {
            self.card_done(id, c);
        }
    }

    fn card_closed(&self, id: &str, card_id: u64) {
        let mut g = self.inner.lock().unwrap();
        g.cards.remove(&(id.to_string(), card_id));
        let open = g.cards.keys().any(|(t, _)| t == id);
        drop(g);
        if !open && let Some(sup) = self.sup.upgrade() {
            self.apply(&sup, id, Event::LastCardAnswered);
        }
    }
}

/// Why a review row's turn end or exit is a crash of the review; `None` for its other events.
fn crash_reason(ev: &Event) -> Option<String> {
    match ev {
        Event::ProcessExit => Some("its process exited".into()),
        Event::TurnUnreported => Some("it ended its turn without a verdict".into()),
        Event::TurnError(e) => Some(format!("its turn ended on: {e}")),
        Event::TurnLimit(l) => Some(format!("{} of provider {}", l.kind.label(), l.provider)),
        Event::GoalEnded(how) => Some(how.clone()),
        _ => None,
    }
}

/// What a `done` report recorded, for the review: commit, files, checks and artifacts.
fn done_facts(f: &serde_json::Map<String, Value>) -> board::Done {
    let strings = |k: &str| -> Vec<String> {
        f.get(k)
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|v| v.as_str().map(String::from))
            .collect()
    };
    let checks = f
        .get("checks")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|c| {
            let code = match &c["exit_code"] {
                Value::Number(n) => format!("exit {n}"),
                _ => "no exit code".into(),
            };
            let folder = c["folder"].as_str().map(|d| format!(" in {d}")).unwrap_or_default();
            let mut line = format!(
                "- `{}`{folder} → {code}{}",
                c["command"].as_str().unwrap_or("?"),
                stale(c)
            );
            if let Some(tail) = c["tail"].as_str().map(str::trim_end).filter(|t| !t.is_empty()) {
                line += &format!("\n  The end of its output:\n~~~~\n{tail}\n~~~~");
            }
            line
        })
        .collect();
    board::Done {
        commit: f.get("commit").and_then(Value::as_str).map(String::from),
        files: strings("files_changed"),
        checks,
        artifacts: strings("artifacts"),
    }
}

/// A task blocked by `provider`'s limit.
fn blocked_on(t: &board::Task, provider: &str) -> bool {
    t.state == State::Blocked
        && t.block
            .as_ref()
            .and_then(|b| b.limit.as_ref())
            .is_some_and(|l| l.provider == provider)
}

fn until(sup: &Super, provider: &str) -> String {
    let b = sup.board.lock().unwrap();
    match b.holds.get(provider).and_then(|l| l.reset_at) {
        Some(at) => format!(
            " for about {} more minutes",
            at.saturating_sub(board::now()).div_ceil(60)
        ),
        None => String::new(),
    }
}

/// The wake line for a board event on task `t` (after it was applied).
fn wake_text(t: &board::Task, ev: &Event) -> String {
    let reason = t.block.as_ref().map(|b| b.reason.clone()).unwrap_or_default();
    let mut s = match ev {
        Event::Report { state, summary, .. } => {
            let st = match state {
                ReportState::Done => "done",
                ReportState::Blocked => "blocked",
                ReportState::Failed => "failed",
                ReportState::Progress => "progress",
            };
            let mut s = format!("{} reported {st} (now {}): {summary}", t.label, t.state.name());
            if *state == ReportState::Done && t.state == State::Ready && t.approval_covers_done() {
                s += &format!(
                    "\nThe earlier approval of commit {} still covers it: no new review.",
                    t.approved_commit.as_deref().unwrap_or_default()
                );
            }
            if t.state == State::Reviewing && t.reviewer.is_none() {
                s += "\nNo review runs for a read task: judge its answer now, with `task accept` or `task review`; \
nothing else will wake you about it.";
            }
            s
        }
        Event::MergeReport(m) => match m {
            MergeResult::Merged => format!(
                "{} merged (now {}). If the user already said to close it after the merge, call `task close` now (its \
card is the confirmation); otherwise offer to close it.",
                t.label,
                t.state.name()
            ),
            MergeResult::PrOpened => format!("{} opened a pull request (now {}).", t.label, t.state.name()),
            MergeResult::Failed(why) => format!("{} merge failed (now {}): {why}", t.label, t.state.name()),
        },
        Event::TurnUnreported => format!("{} is blocked (code-made report): {reason}.", t.label),
        Event::TurnError(_) => format!("{} is blocked (code-made report): its turn ended on: {reason}", t.label),
        Event::TurnLimit(l) => format!(
            "{} is blocked (code-made report): {} of provider {}; code resends when it clears.",
            t.label,
            l.kind.label(),
            l.provider
        ),
        Event::GoalEnded(_) => format!("{} is blocked (code-made report): {reason}.", t.label),
        Event::ProcessExit if t.state == State::Failed => {
            format!("{} failed: its process exited while it was working.", t.label)
        }
        Event::ProcessExit => format!(
            "{} is {}: its process exited while it was merging.",
            t.label,
            t.state.name()
        ),
        Event::Verdict(board::Verdict::Approved) => {
            let mut s = format!(
                "{}'s review approved it (now {}). If its findings or checks don't support the approval, ask for a \
second review with `task review`.",
                t.label,
                t.state.name()
            );
            if t.write && t.repo.is_none() && t.state == State::Ready {
                s +=
                    " It is not in git, so there is nothing to merge: ask the user whether to close it (`task close`).";
            }
            s
        }
        Event::Verdict(v) if t.state == State::Blocked => format!(
            "{}'s review says {} and its review rounds are used up (now blocked). Ask the user how to go on; \
if they want it as it is, `task accept` asks them on a card to accept it without an approved review.",
            t.label,
            v.name()
        ),
        Event::Verdict(v) => format!(
            "{}'s review says {} (round {} of 2): code sent the findings to the task (now {}).",
            t.label,
            v.name(),
            t.review_rounds,
            t.state.name()
        ),
        Event::BranchMoved => format!(
            "{}'s branch moved during its review: the verdict is discarded and it is {} again; its next `done` \
starts a new review.",
            t.label,
            t.state.name()
        ),
        Event::ReviewCrashed(_) | Event::ReviewCantStart(_) => {
            format!("{} is blocked: {reason}. Ask the user what to do.", t.label)
        }
        _ => format!("{} is {}.", t.label, t.state.name()),
    };
    if let Some(q) = &t.question {
        s += &format!("\nIt asks: {q}");
    }
    s
}

/// The mark on a reported check that ran before the task's last edit.
fn stale(c: &Value) -> &'static str {
    if c["before_last_edit"] == true {
        " (before the last edit)"
    } else {
        ""
    }
}

/// The checks of a report, one line each, for super to judge.
fn checks_text(f: &serde_json::Map<String, Value>) -> String {
    let mut out = Vec::new();
    for c in f.get("checks").and_then(Value::as_array).into_iter().flatten() {
        let cmd = c["command"].as_str().unwrap_or("?");
        let code = match &c["exit_code"] {
            Value::Number(n) => format!("exit {n}"),
            _ => "no exit code".into(),
        };
        let last = c["last_line"].as_str().unwrap_or("");
        out.push(format!(
            "- #{} `{cmd}` → {code}{}{}",
            c["n"],
            stale(c),
            if last.is_empty() {
                String::new()
            } else {
                format!(": {last}")
            }
        ));
    }
    for a in f.get("artifacts").and_then(Value::as_array).into_iter().flatten() {
        out.push(format!(
            "- artifact {}",
            a.as_str().map_or(a.to_string(), str::to_string)
        ));
    }
    if let Some(files) = f.get("files_changed").and_then(Value::as_array) {
        out.push(format!("- {} files changed", files.len()));
    }
    if let Some(c) = f.get("commit").and_then(Value::as_str) {
        out.push(format!("- commit {c}"));
    }
    if out.is_empty() {
        String::new()
    } else {
        format!("\nChecks:\n{}", out.join("\n"))
    }
}

/// The wake lines for a code-made report of a turn that ended without one: the
/// task's last message, so an answer given in text isn't lost.
fn said(w: &Watcher, ev: &Event) -> String {
    match (ev, w.last_text()) {
        (Event::TurnUnreported | Event::GoalEnded(_), Some(t)) => format!("\nIts last message:\n{t}"),
        _ => String::new(),
    }
}

/// One task's link: connect, read its messages, reconnect while its process lives.
async fn run_link(
    me: Weak<Links>,
    id: String,
    session: String,
    writer: Arc<tokio::sync::Mutex<Option<Box<dyn AsyncWrite + Send + Unpin>>>>,
) {
    let mut watcher = {
        let Some(l) = me.upgrade() else { return };
        let Some(sup) = l.sup.upgrade() else { return };
        let model = sup.row(&id).map(|t| t.model).unwrap_or_default();
        let mut w = Watcher::new(super::task::provider(&model));
        w.goal(Goal::read(sup.home(), &session));
        w
    };
    loop {
        let conn = {
            let Some(l) = me.upgrade() else { return };
            let Some(sup) = l.sup.upgrade() else { return };
            if !sup.launcher.alive(&session) {
                l.exited(&id, &session);
                return;
            }
            sup.launcher.link(&session).await
        };
        let Ok(Conn { read, mut write }) = conn else {
            tokio::time::sleep(Duration::from_millis(500)).await;
            continue;
        };
        // Its response comes after the catch-up lines (hello, history, open cards).
        let ask = Request {
            id: Some("link".into()),
            cmd: ClientCmd::GetState,
        };
        if write.write_all(line(&ask).as_bytes()).await.is_err() {
            continue;
        }
        let _ = write.flush().await;
        *writer.lock().await = Some(write);
        let mut lines = BufReader::new(read).lines();
        let mut history = Vec::new();
        // Cards the catch-up re-sent: super's copies of any others were answered meanwhile.
        let mut open = HashSet::new();
        let mut busy = false;
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        loop {
            let l = tokio::select! {
                l = lines.next_line() => l,
                _ = tick.tick() => {
                    let Some(links) = me.upgrade() else { return };
                    let Some(sup) = links.sup.upgrade() else { return };
                    if let Some(ev) = watcher.goal(Goal::read(sup.home(), &session)) {
                        links.apply(&sup, &id, ev);
                    }
                    continue;
                }
            };
            let Ok(Some(l)) = l else { break };
            let Ok(msg) = serde_json::from_str::<ServerMsg>(&l) else {
                continue;
            };
            let Some(links) = me.upgrade() else { return };
            let Some(sup) = links.sup.upgrade() else { return };
            match msg {
                ServerMsg::Hello { state, .. } | ServerMsg::State { state } => {
                    busy = state.busy;
                    watcher.state(&state);
                }
                ServerMsg::History { messages } => history = messages,
                ServerMsg::Response { id: Some(r), .. } if r == "link" => {
                    links.cards_gone(&id, &open);
                    let last = sup.row(&id).ok().and_then(|t| t.last_prompt);
                    let goal = Goal::read(sup.home(), &session);
                    match watcher.catch_up(&history, last.as_deref(), busy, goal) {
                        CatchUp::Nothing => {}
                        CatchUp::Report(ev) => {
                            if links.apply_with(&sup, &id, ev.clone(), &said(&watcher, &ev)) {
                                links.kick();
                            }
                        }
                        CatchUp::Note => {
                            let label = sup.row(&id).map(|t| t.label).unwrap_or_default();
                            links.inform.note(format!(
                                "{label}: a turn may have ended while super was disconnected, but the user also \
                                 typed in it, so no report was made; ask it with `task send` if needed"
                            ));
                        }
                    }
                    links.replied(&sup, &id, &mut watcher);
                    history = Vec::new();
                }
                ServerMsg::Event { event } => {
                    if let AgentEvent::AgentStart = event {
                        busy = true;
                    }
                    let home = sup.home().to_path_buf();
                    let ev = watcher.event(&event, || Goal::read(&home, &session));
                    if let Some(ev) = ev
                        && links.apply_with(&sup, &id, ev.clone(), &said(&watcher, &ev))
                    {
                        links.kick();
                    }
                    links.replied(&sup, &id, &mut watcher);
                }
                ServerMsg::Ask { .. } | ServerMsg::Approve { .. } => {
                    if let Some(card) = Card::from_msg(&msg) {
                        open.insert(card.id());
                        links.card(&id, card);
                    }
                }
                ServerMsg::ApprovalDone { id: c } | ServerMsg::AskDone { id: c } => links.card_done(&id, c),
                ServerMsg::Bye { .. } => break,
                _ => {}
            }
        }
        *writer.lock().await = None;
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// The real [`Inform`]: super's own session, through its hub.
pub struct HubInform {
    pub slot: crate::hub::HubSlot,
    /// `notifications.enabled`.
    pub desktop: bool,
}

impl HubInform {
    /// `f` on super's session now, or once its hub starts (restart recovery runs before it).
    fn handle(&self, f: impl FnOnce(novacode_core::agent::SessionHandle) + Send + 'static) {
        match self.slot.0.get() {
            Some(w) => {
                if let Some(h) = w.upgrade() {
                    f(h.handle.clone())
                }
            }
            None => {
                let slot = self.slot.clone();
                tokio::spawn(async move {
                    if let Some(h) = slot.hub().await {
                        f(h.handle.clone())
                    }
                });
            }
        }
    }
}

impl Inform for HubInform {
    fn wake(&self, text: String) {
        self.handle(|h| h.remind_as("super", text, true));
    }
    fn note(&self, text: String) {
        self.handle(|h| h.remind_as("super", text, false));
    }
    fn notice(&self, text: String) {
        self.handle(|h| h.emit(AgentEvent::Stream(novacode_types::StreamEvent::Notice { text })));
    }
    fn desktop(&self, title: String, body: String) {
        if self.desktop {
            tokio::spawn(crate::activity::desktop(title, body));
        }
    }
    fn closing(&self) -> bool {
        // Not started yet is not closing.
        self.slot
            .0
            .get()
            .is_some_and(|w| w.upgrade().is_none_or(|h| h.is_closed()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::superagent::Tier;
    use crate::superagent::board::{Kind, Task};
    use crate::superagent::task::{Config, Launcher, Start};
    use futures_util::future::BoxFuture;
    use novacode_core::policy::{Approval, ApprovalRequest};
    use novacode_types::{AssistantMessage, Block, Question, Reply, Usage};
    use serde_json::json;
    use tokio::io::DuplexStream;

    // ---- The watcher alone ----

    fn end(prompts: &[Option<&str>], stop: StopReason) -> AgentEvent {
        AgentEvent::AgentEnd {
            stop,
            prompts: prompts.iter().map(|p| p.map(String::from)).collect(),
        }
    }
    const SUPER: &[Option<&str>] = &[Some("super")];

    fn report(state: &str) -> AgentEvent {
        AgentEvent::Message(Message::ToolResult {
            call_id: "c".into(),
            tool_name: "report".into(),
            content: vec![],
            is_error: false,
            details: Some(json!({"report": {"state": state, "summary": "s"}})),
        })
    }

    fn assistant(stop: StopReason, error: Option<&str>) -> Message {
        Message::Assistant(AssistantMessage {
            content: vec![Block::text("x")],
            stop,
            usage: Usage::default(),
            provider: "faux".into(),
            model: "m".into(),
            response_id: None,
            error_message: error.map(String::from),
            thinking_level: None,
        })
    }

    /// Feed `events` as one run; the result of its end.
    fn run(w: &mut Watcher, events: &[AgentEvent], goal: Goal) -> Option<Event> {
        let mut out = None;
        w.event(&AgentEvent::AgentStart, || goal);
        for e in events {
            out = w.event(e, || goal);
        }
        out
    }

    fn says(text: &str) -> AgentEvent {
        AgentEvent::Message(Message::Assistant(AssistantMessage {
            content: vec![Block::text(text)],
            ..match assistant(StopReason::Stop, None) {
                Message::Assistant(a) => a,
                _ => unreachable!(),
            }
        }))
    }

    #[test]
    fn a_code_made_report_carries_the_task_s_last_text_cut() {
        let mut w = Watcher::new("faux");
        let ev = run(
            &mut w,
            &[says("checking"), says("merged: abc123"), end(SUPER, StopReason::Stop)],
            Goal::None,
        );
        assert_eq!(ev, Some(Event::TurnUnreported));
        assert_eq!(w.last_text().as_deref(), Some("merged: abc123"));
        // A new run starts empty; a long answer is cut.
        run(&mut w, &[says(&"é".repeat(700))], Goal::None);
        let cut = w.last_text().unwrap();
        assert!(cut.chars().count() <= 601 && cut.ends_with('…'), "{cut}");
        run(&mut w, &[], Goal::None);
        assert_eq!(w.last_text(), None);
    }

    const QUESTION: &[Option<&str>] = &[Some(SUPER_QUESTION_ID)];

    #[test]
    fn a_question_run_ends_with_a_reply_not_a_code_made_report() {
        assert_ne!(SUPER_QUESTION_ID, SUPER_ID);
        let mut w = Watcher::new("faux");
        let ev = run(
            &mut w,
            &[says("check 9: exit 0"), end(QUESTION, StopReason::Stop)],
            Goal::None,
        );
        assert_eq!(ev, None);
        assert_eq!(w.take_reply().as_deref(), Some("check 9: exit 0"));
        assert_eq!(w.take_reply(), None, "taken once");
        // Errors still block, with no reply.
        let ev = run(
            &mut w,
            &[
                AgentEvent::Message(assistant(StopReason::Length, None)),
                end(QUESTION, StopReason::Length),
            ],
            Goal::None,
        );
        assert!(matches!(ev, Some(Event::TurnError(_))), "{ev:?}");
        assert_eq!(w.take_reply(), None);
        // Super's plain prompt is unchanged.
        assert_eq!(
            run(&mut w, &[end(SUPER, StopReason::Stop)], Goal::None),
            Some(Event::TurnUnreported)
        );
        assert_eq!(w.take_reply(), None);
    }

    #[test]
    fn catch_up_treats_a_question_prompt_as_super_s_own_without_a_block() {
        let q = question_prompt("paste check #9");
        let hash = board::prompt_hash(&q);
        let ended = vec![Message::user(&q), assistant(StopReason::Stop, None)];
        let mut w = Watcher::new("faux");
        assert_eq!(w.catch_up(&ended, Some(&hash), false, Goal::None), CatchUp::Nothing);
        assert_eq!(w.take_reply().as_deref(), Some("x"));
    }

    #[test]
    fn a_turn_on_super_s_prompt_without_a_terminal_report_is_blocked() {
        let mut w = Watcher::new("faux");
        let none = run(&mut w, &[end(SUPER, StopReason::Stop)], Goal::None);
        assert_eq!(none, Some(Event::TurnUnreported));
        // A progress report doesn't count.
        let progress = run(&mut w, &[report("progress"), end(SUPER, StopReason::Stop)], Goal::None);
        assert_eq!(progress, Some(Event::TurnUnreported));
        for state in ["done", "blocked", "failed"] {
            let r = run(&mut w, &[report(state), end(SUPER, StopReason::Stop)], Goal::None);
            assert_eq!(r, None, "{state}");
        }
        // A refused report call is no report.
        let refused = AgentEvent::Message(Message::ToolResult {
            call_id: "c".into(),
            tool_name: "report".into(),
            content: vec![],
            is_error: true,
            details: None,
        });
        assert_eq!(
            run(&mut w, &[refused, end(SUPER, StopReason::Stop)], Goal::None),
            Some(Event::TurnUnreported)
        );
    }

    #[test]
    fn user_prompts_runs_without_a_wire_prompt_stops_and_goal_turns_make_no_report() {
        let mut w = Watcher::new("faux");
        for prompts in [&[None][..], &[Some("super"), None], &[]] {
            assert_eq!(
                run(&mut w, &[end(prompts, StopReason::Stop)], Goal::None),
                None,
                "{prompts:?}"
            );
        }
        assert_eq!(run(&mut w, &[end(SUPER, StopReason::Aborted)], Goal::None), None);
        assert_eq!(run(&mut w, &[end(SUPER, StopReason::Stop)], Goal::Active), None);
    }

    #[test]
    fn error_and_length_ends_are_blocked_with_the_error_text() {
        let mut w = Watcher::new("faux");
        let err = StopReason::Error {
            kind: ErrorKind::Overflow,
        };
        let r = run(
            &mut w,
            &[
                AgentEvent::Message(assistant(err.clone(), Some("context window exceeded"))),
                end(SUPER, err),
            ],
            Goal::None,
        );
        assert_eq!(r, Some(Event::TurnError("context window exceeded".into())));
        let r = run(&mut w, &[end(SUPER, StopReason::Length)], Goal::None);
        assert!(matches!(r, Some(Event::TurnError(t)) if t.contains("length limit")));
    }

    #[test]
    fn quota_and_rate_limit_ends_block_on_the_task_s_provider() {
        let mut w = Watcher::new("claude");
        let quota = StopReason::Error {
            kind: ErrorKind::QuotaExhausted,
        };
        let r = run(&mut w, &[end(SUPER, quota)], Goal::None);
        assert_eq!(
            r,
            Some(Event::TurnLimit(Limit {
                kind: LimitKind::Quota,
                provider: "claude".into(),
                reset_at: None
            }))
        );
        // The reset time comes from the retries when the end has none.
        let rl = ErrorKind::RateLimit { retry_after_ms: None };
        let retry = AgentEvent::Retry {
            attempt: 1,
            delay_ms: 1,
            error: ErrorKind::RateLimit {
                retry_after_ms: Some(120_000),
            },
            message: "429".into(),
        };
        let before = board::now();
        let Some(Event::TurnLimit(l)) = run(&mut w, &[retry, end(SUPER, StopReason::Error { kind: rl })], Goal::None)
        else {
            panic!()
        };
        assert_eq!((l.kind, l.provider.as_str()), (LimitKind::RateLimit, "claude"));
        let at = l.reset_at.unwrap();
        assert!((before + 120..=before + 122).contains(&at), "{at}");
    }

    #[test]
    fn background_work_defers_the_check_to_a_later_turn_end_with_none() {
        let mut w = Watcher::new("faux");
        let started = AgentEvent::ProcessStarted {
            id: "p1".into(),
            name: "cargo test".into(),
        };
        let ended = AgentEvent::ProcessEnded {
            id: "p1".into(),
            name: "cargo test".into(),
            code: Some(0),
            stopped: false,
        };
        assert_eq!(run(&mut w, &[started, end(SUPER, StopReason::Stop)], Goal::None), None);
        // A wake while it still runs: nothing yet.
        w.event(&ended, || Goal::None);
        assert_eq!(
            run(&mut w, &[end(&[], StopReason::Stop)], Goal::None),
            Some(Event::TurnUnreported)
        );
        // Once only.
        assert_eq!(run(&mut w, &[end(&[], StopReason::Stop)], Goal::None), None);

        // A subagent (the Agent tab's node) running at the end; the user types before it ends.
        let sub = |dim: bool| AgentEvent::Ui {
            ext: "agents".into(),
            op: novacode_types::ui::UiOp::Publish {
                tab: "agent".into(),
                source: "subagents".into(),
                node: Box::new(novacode_types::ui::Node {
                    id: "a1".into(),
                    parent: None,
                    label: "scout".into(),
                    tag: None,
                    status: None,
                    icon: None,
                    dim,
                    detail: vec![],
                    time: None,
                    order: None,
                    actions: Default::default(),
                }),
            },
        };
        assert_eq!(
            run(&mut w, &[sub(false), end(SUPER, StopReason::Stop)], Goal::None),
            None
        );
        assert_eq!(
            run(&mut w, &[end(&[], StopReason::Stop)], Goal::None),
            None,
            "still running"
        );
        w.event(&sub(true), || Goal::None);
        assert_eq!(
            run(&mut w, &[end(&[None], StopReason::Stop)], Goal::None),
            None,
            "the user typed"
        );
        assert_eq!(run(&mut w, &[end(&[], StopReason::Stop)], Goal::None), None);

        // A terminal report in between also cancels it.
        let p = |e: &str| AgentEvent::ProcessStarted {
            id: e.into(),
            name: "x".into(),
        };
        run(&mut w, &[p("p2"), end(SUPER, StopReason::Stop)], Goal::None);
        w.event(
            &AgentEvent::ProcessEnded {
                id: "p2".into(),
                name: "x".into(),
                code: Some(0),
                stopped: false,
            },
            || Goal::None,
        );
        assert_eq!(
            run(&mut w, &[report("done"), end(&[], StopReason::Stop)], Goal::None),
            None
        );
    }

    #[test]
    fn a_goal_that_ends_unreported_is_blocked_and_a_paused_one_is_not() {
        let mut w = Watcher::new("faux");
        assert_eq!(w.goal(Goal::None), None);
        assert_eq!(w.goal(Goal::Active), None);
        assert_eq!(run(&mut w, &[end(SUPER, StopReason::Stop)], Goal::Active), None);
        assert_eq!(
            w.goal(Goal::Done),
            Some(Event::GoalEnded("goal done without a report".into()))
        );
        assert_eq!(w.goal(Goal::Done), None, "once");
        w.goal(Goal::Active);
        assert_eq!(w.goal(Goal::Paused), None);
        w.goal(Goal::Active);
        assert_eq!(w.goal(Goal::Blocked), Some(Event::GoalEnded("goal blocked".into())));
        // Reported during the goal's own runs: no code-made report.
        w.goal(Goal::Active);
        run(&mut w, &[report("done"), end(&[], StopReason::Stop)], Goal::Active);
        assert_eq!(w.goal(Goal::Failed), None);
    }

    #[test]
    fn a_goal_that_ends_mid_run_is_judged_at_the_run_s_end() {
        // R18: the goal is done mid-run, the task reports `done` later in the same run.
        for runs in [SUPER, &[] as &[Option<&str>]] {
            let mut w = Watcher::new("faux");
            w.goal(Goal::Active);
            w.event(&AgentEvent::AgentStart, || Goal::Active);
            assert_eq!(w.goal(Goal::Done), None, "not judged mid-run");
            assert_eq!(w.event(&report("done"), || Goal::Done), None);
            assert_eq!(w.event(&end(runs, StopReason::Stop), || Goal::Done), None);
            assert_eq!(w.goal(Goal::Done), None);

            // Reported first, then the goal ends in the same run.
            let mut w = Watcher::new("faux");
            w.goal(Goal::Active);
            w.event(&AgentEvent::AgentStart, || Goal::Active);
            w.event(&report("done"), || Goal::Active);
            assert_eq!(w.goal(Goal::Done), None);
            assert_eq!(w.event(&end(runs, StopReason::Stop), || Goal::Done), None);
            assert_eq!(w.goal(Goal::Done), None);

            // No report by the run's end: the goal end is reported then, once.
            let mut w = Watcher::new("faux");
            w.goal(Goal::Active);
            w.event(&AgentEvent::AgentStart, || Goal::Active);
            assert_eq!(w.goal(Goal::Done), None);
            assert_eq!(
                w.event(&end(runs, StopReason::Stop), || Goal::Done),
                Some(Event::GoalEnded("goal done without a report".into()))
            );
            assert_eq!(w.goal(Goal::Done), None, "once");
        }
    }

    #[test]
    fn catch_up_reports_only_when_super_s_prompt_was_the_only_one() {
        let brief = "Task T1: do it";
        let hash = board::prompt_hash(brief);
        let ended = vec![Message::user(brief), assistant(StopReason::Stop, None)];
        let mut w = Watcher::new("faux");
        assert_eq!(
            w.catch_up(&ended, Some(&hash), false, Goal::None),
            CatchUp::Report(Event::TurnUnreported)
        );
        assert_eq!(
            w.catch_up(&ended, Some(&hash), true, Goal::None),
            CatchUp::Nothing,
            "busy"
        );
        assert_eq!(w.catch_up(&ended, Some(&hash), false, Goal::Active), CatchUp::Nothing);
        assert_eq!(w.catch_up(&ended, None, false, Goal::None), CatchUp::Nothing);
        // Not delivered yet, or not answered yet.
        assert_eq!(
            w.catch_up(&ended[1..], Some(&hash), false, Goal::None),
            CatchUp::Nothing
        );
        assert_eq!(
            w.catch_up(&ended[..1], Some(&hash), false, Goal::None),
            CatchUp::Nothing
        );
        let mut typed = ended.clone();
        typed.push(Message::user("also do this"));
        typed.push(assistant(StopReason::Stop, None));
        assert_eq!(w.catch_up(&typed, Some(&hash), false, Goal::None), CatchUp::Note);
        let AgentEvent::Message(r) = report("blocked") else {
            panic!()
        };
        let reported = vec![ended[0].clone(), r, ended[1].clone()];
        assert_eq!(w.catch_up(&reported, Some(&hash), false, Goal::None), CatchUp::Nothing);
        let failed = vec![
            ended[0].clone(),
            assistant(StopReason::Error { kind: ErrorKind::Auth }, Some("401")),
        ];
        assert_eq!(
            w.catch_up(&failed, Some(&hash), false, Goal::None),
            CatchUp::Report(Event::TurnError("401".into()))
        );
    }

    #[test]
    fn catch_up_note_is_given_once_per_state() {
        let brief = "Task T1: do it";
        let hash = board::prompt_hash(brief);
        let mut typed = vec![
            Message::user(brief),
            assistant(StopReason::Stop, None),
            Message::user("also do this"),
            assistant(StopReason::Stop, None),
        ];
        let mut w = Watcher::new("faux");
        assert_eq!(w.catch_up(&typed, Some(&hash), false, Goal::None), CatchUp::Note);
        assert_eq!(
            w.catch_up(&typed, Some(&hash), false, Goal::None),
            CatchUp::Nothing,
            "same state on the next reconnect"
        );
        typed.push(Message::user("and this"));
        typed.push(assistant(StopReason::Stop, None));
        assert_eq!(
            w.catch_up(&typed, Some(&hash), false, Goal::None),
            CatchUp::Note,
            "new activity"
        );
        let brief2 = "Task T1: next";
        let hash2 = board::prompt_hash(brief2);
        typed.push(Message::user(brief2));
        typed.push(Message::user("typed again"));
        typed.push(assistant(StopReason::Stop, None));
        assert_eq!(
            w.catch_up(&typed, Some(&hash2), false, Goal::None),
            CatchUp::Note,
            "new prompt"
        );
    }

    // ---- Links against fake task sockets ----

    type Log = Arc<Mutex<Vec<String>>>;

    #[derive(Default)]
    struct FakeLauncher {
        alive: Mutex<HashSet<String>>,
        sent: Mutex<Vec<(String, ClientCmd)>>,
        starts: Mutex<Vec<Start>>,
        /// Task sides of the links, by session.
        conns: Mutex<Vec<(String, DuplexStream)>>,
    }

    impl Launcher for FakeLauncher {
        fn start(&self, s: Start) -> BoxFuture<'_, Result<String, String>> {
            Box::pin(async move {
                let mut starts = self.starts.lock().unwrap();
                let id = format!("new{}", starts.len() + 1);
                starts.push(s);
                self.alive.lock().unwrap().insert(id.clone());
                Ok(id)
            })
        }
        fn send_as<'a>(&'a self, session: &'a str, _id: &'a str, cmd: ClientCmd) -> BoxFuture<'a, Result<(), String>> {
            Box::pin(async move {
                self.sent.lock().unwrap().push((session.into(), cmd));
                Ok(())
            })
        }
        fn alive(&self, session: &str) -> bool {
            self.alive.lock().unwrap().contains(session)
        }
        fn link<'a>(&'a self, session: &'a str) -> BoxFuture<'a, Result<Conn, String>> {
            Box::pin(async move {
                let (ours, theirs) = tokio::io::duplex(1 << 16);
                self.conns.lock().unwrap().push((session.into(), theirs));
                let (r, w) = tokio::io::split(ours);
                Ok(Conn {
                    read: Box::new(r),
                    write: Box::new(w),
                })
            })
        }
    }

    /// The task's end of a link.
    struct TaskEnd {
        lines: tokio::io::Lines<BufReader<tokio::io::ReadHalf<DuplexStream>>>,
        w: tokio::io::WriteHalf<DuplexStream>,
    }

    impl TaskEnd {
        async fn put(&mut self, m: ServerMsg) {
            self.w.write_all(line(&m).as_bytes()).await.unwrap();
        }
        async fn event(&mut self, e: AgentEvent) {
            self.put(ServerMsg::Event { event: e }).await;
        }
        /// The next command super sent on the link.
        async fn cmd(&mut self) -> Request {
            let l = tokio::time::timeout(Duration::from_secs(5), self.lines.next_line())
                .await
                .expect("no command")
                .unwrap()
                .unwrap();
            serde_json::from_str(&l).unwrap()
        }
        /// Hello, history and open cards, then the answer to the link's `get_state`.
        async fn catch_up(&mut self, busy: bool, history: Vec<Message>, cards: Vec<ServerMsg>) {
            let state = crate::wire::State {
                busy,
                ..Default::default()
            };
            self.put(ServerMsg::Hello {
                protocol: 1,
                version: "t".into(),
                capabilities: vec![],
                state,
            })
            .await;
            self.put(ServerMsg::History { messages: history }).await;
            for c in cards {
                self.put(c).await;
            }
            let r = self.cmd().await;
            assert_eq!((r.id.as_deref(), r.cmd.name()), (Some("link"), "get_state"));
            self.put(ServerMsg::Response {
                id: Some("link".into()),
                command: "get_state".into(),
                ok: true,
                data: Value::Null,
                error: None,
            })
            .await;
        }
    }

    /// Asks wait until the test answers; a dropped wait (a closed copy) is seen as closed.
    #[derive(Default)]
    struct FakeAsker {
        asked: Mutex<Vec<Question>>,
        waiting: Mutex<Vec<tokio::sync::oneshot::Sender<Option<Vec<Reply>>>>>,
    }

    impl Asker for FakeAsker {
        fn ask(&self, questions: Vec<Question>) -> BoxFuture<'_, Option<Vec<Reply>>> {
            let (tx, rx) = tokio::sync::oneshot::channel();
            self.asked.lock().unwrap().extend(questions);
            self.waiting.lock().unwrap().push(tx);
            Box::pin(async move { rx.await.ok().flatten() })
        }
    }

    struct Allow;
    impl Approver for Allow {
        fn approve(&self, req: ApprovalRequest) -> BoxFuture<'_, Approval> {
            assert!(req.reason.starts_with("[T1-x] "), "{}", req.reason);
            Box::pin(async { Approval::Once })
        }
    }

    #[derive(Default)]
    struct Said(Log, Arc<std::sync::atomic::AtomicBool>);
    impl Inform for Said {
        fn closing(&self) -> bool {
            self.1.load(std::sync::atomic::Ordering::Relaxed)
        }
        fn wake(&self, t: String) {
            self.0.lock().unwrap().push(format!("wake: {t}"));
        }
        fn note(&self, t: String) {
            self.0.lock().unwrap().push(format!("note: {t}"));
        }
        fn notice(&self, t: String) {
            self.0.lock().unwrap().push(format!("notice: {t}"));
        }
        fn desktop(&self, title: String, _: String) {
            self.0.lock().unwrap().push(format!("desktop: {title}"));
        }
    }

    struct Fx {
        _tmp: tempfile::TempDir,
        sup: Arc<Super>,
        links: Arc<Links>,
        launcher: Arc<FakeLauncher>,
        asker: Arc<FakeAsker>,
        log: Log,
        closing: Arc<std::sync::atomic::AtomicBool>,
    }

    fn fixture(max_running: usize) -> Fx {
        fixture_with(max_running, |_, _| {})
    }

    /// `before` fills the board and the live sessions before the links start, as a
    /// board found by a restarted super.
    fn fixture_with(max_running: usize, before: impl FnOnce(&Super, &FakeLauncher)) -> Fx {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().canonicalize().unwrap();
        let (root, home) = (base.join("root"), base.join("home"));
        std::fs::create_dir_all(&root).unwrap();
        let launcher = Arc::new(FakeLauncher::default());
        let asker = Arc::new(FakeAsker::default());
        let cfg = Config {
            max_running,
            tiers: vec![(
                "standard".into(),
                Tier {
                    model: "faux/s".into(),
                    note: String::new(),
                },
            )],
            worktree_copy: vec![],
            home: home.clone(),
            allow: Arc::new(|_: &str| Ok(())),
        };
        let sup =
            Arc::new(Super::new(&root, home.join("super/x"), cfg, Some(asker.clone()), launcher.clone()).unwrap());
        before(&sup, &launcher);
        let log = Log::default();
        let closing = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let said = Said(log.clone(), closing.clone());
        let links = Links::start(&sup, asker.clone(), Arc::new(Allow), Arc::new(said));
        Fx {
            _tmp: tmp,
            sup,
            links,
            launcher,
            asker,
            log,
            closing,
        }
    }

    impl Fx {
        /// A row in `state` with a live session `s<n>`, as if started earlier.
        fn task(&self, label: &str, model: &str, state: State) -> String {
            let session = format!("s-{label}");
            self.launcher.alive.lock().unwrap().insert(session.clone());
            let t = Task {
                id: label.into(),
                label: label.into(),
                kind: Kind::Work,
                model: model.into(),
                token: "tok".into(),
                state,
                session: Some(session.clone()),
                cwd: Some(self.sup.root.clone()),
                request: "r".into(),
                ..Task::default()
            };
            self.sup.edit(|b| b.tasks.push(t)).unwrap();
            session
        }

        async fn end(&self, session: &str) -> TaskEnd {
            for _ in 0..300 {
                let conn = {
                    let mut g = self.launcher.conns.lock().unwrap();
                    g.iter().position(|(s, _)| s == session).map(|i| g.remove(i).1)
                };
                if let Some(c) = conn {
                    let (r, w) = tokio::io::split(c);
                    return TaskEnd {
                        lines: BufReader::new(r).lines(),
                        w,
                    };
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            panic!("{session} was never linked");
        }

        fn state(&self, id: &str) -> State {
            self.sup.row(id).unwrap().state
        }

        fn said(&self) -> Vec<String> {
            self.log.lock().unwrap().clone()
        }

        async fn until(&self, what: &str, f: impl Fn(&Fx) -> bool) {
            for _ in 0..300 {
                if f(self) {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            panic!("never: {what}; said {:?}", self.said());
        }
    }

    fn ask(id: u64) -> ServerMsg {
        ServerMsg::Ask {
            id,
            questions: vec![Question {
                question: "which db?".into(),
                header: None,
                options: vec![],
                multi: false,
            }],
        }
    }

    #[tokio::test]
    async fn a_task_s_question_is_relayed_with_its_label_once_and_its_answer_goes_back() {
        let fx = fixture(5);
        let s = fx.task("T1-x", "faux/a", State::Running);
        let mut t = fx.end(&s).await;
        t.catch_up(false, vec![], vec![ask(7)]).await;
        fx.until("asked", |fx| fx.asker.waiting.lock().unwrap().len() == 1)
            .await;
        assert_eq!(fx.asker.asked.lock().unwrap()[0].question, "[T1-x] which db?");
        assert_eq!(fx.state("T1-x"), State::WaitingYou);
        assert!(fx.said().contains(&"desktop: T1-x asks".to_string()));

        // The link drops and comes back: the hub re-sends the open card with its id.
        drop(t);
        let mut t = fx.end(&s).await;
        t.catch_up(false, vec![], vec![ask(7)]).await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(fx.asker.asked.lock().unwrap().len(), 1, "no duplicate card");

        let tx = fx.asker.waiting.lock().unwrap().remove(0);
        let reply = Reply {
            question: "which db?".into(),
            chosen: vec!["sqlite".into()],
            text: None,
            preview: None,
        };
        tx.send(Some(vec![reply])).unwrap();
        let r = t.cmd().await;
        let ClientCmd::Answer { ask, replies } = r.cmd else {
            panic!("{:?}", r.cmd)
        };
        assert_eq!((ask, replies.unwrap()[0].chosen[0].as_str()), (7, "sqlite"));
        fx.until("running again", |fx| fx.state("T1-x") == State::Running).await;
        let said = fx.said();
        assert!(
            said.iter()
                .any(|l| l == "note: T1-x asked which db?; you answered: sqlite"),
            "{said:?}"
        );
        assert!(!said.iter().any(|l| l.starts_with("wake:")), "cards never wake super");
    }

    #[tokio::test]
    async fn a_card_answered_in_the_task_window_closes_super_s_copy_and_approvals_relay_too() {
        let fx = fixture(5);
        let s = fx.task("T1-x", "faux/a", State::Running);
        let mut t = fx.end(&s).await;
        t.catch_up(false, vec![], vec![ask(3)]).await;
        fx.until("asked", |fx| fx.asker.waiting.lock().unwrap().len() == 1)
            .await;
        t.put(ServerMsg::AskDone { id: 3 }).await;
        fx.until("copy closed", |fx| fx.asker.waiting.lock().unwrap()[0].is_closed())
            .await;
        fx.until("running", |fx| fx.state("T1-x") == State::Running).await;

        t.put(ServerMsg::Approve {
            id: 4,
            tool: "bash".into(),
            detail: "rm -rf target".into(),
            reason: "risky".into(),
            rule: "bash(rm *)".into(),
            risky: true,
        })
        .await;
        let r = t.cmd().await;
        assert!(
            matches!(
                r.cmd,
                ClientCmd::Approval {
                    approval: 4,
                    answer: novacode_tui::dialog::Answer::Once
                }
            ),
            "{:?}",
            r.cmd
        );
    }

    #[tokio::test]
    async fn a_card_answered_while_the_link_was_down_closes_super_s_copy_on_reconnect() {
        let fx = fixture(5);
        let s = fx.task("T1-x", "faux/a", State::Running);
        let mut t = fx.end(&s).await;
        t.catch_up(false, vec![], vec![ask(3), ask(4)]).await;
        fx.until("asked", |fx| fx.asker.waiting.lock().unwrap().len() == 2)
            .await;
        assert_eq!(fx.state("T1-x"), State::WaitingYou);
        // Card 3 is answered in the task's window while the link is down; 4 stays open.
        drop(t);
        let mut t = fx.end(&s).await;
        t.catch_up(false, vec![], vec![ask(4)]).await;
        fx.until("copy of 3 closed", |fx| fx.asker.waiting.lock().unwrap()[0].is_closed())
            .await;
        assert!(!fx.asker.waiting.lock().unwrap()[1].is_closed(), "4 is still open");
        assert_eq!(fx.state("T1-x"), State::WaitingYou);
        // Then 4 too: the task runs again.
        drop(t);
        let mut t = fx.end(&s).await;
        t.catch_up(false, vec![], vec![]).await;
        fx.until("running", |fx| fx.state("T1-x") == State::Running).await;
        assert_eq!(fx.asker.asked.lock().unwrap().len(), 2, "no card opened again");
    }

    #[tokio::test]
    async fn super_shutting_down_never_answers_a_task_s_card() {
        let fx = fixture(5);
        let s = fx.task("T1-x", "faux/a", State::Running);
        let mut t = fx.end(&s).await;
        t.catch_up(false, vec![], vec![ask(3)]).await;
        fx.until("asked", |fx| fx.asker.waiting.lock().unwrap().len() == 1)
            .await;
        // Super's hub closes: its copy of the card ends without an answer.
        fx.closing.store(true, std::sync::atomic::Ordering::Relaxed);
        let tx = fx.asker.waiting.lock().unwrap().remove(0);
        tx.send(None).unwrap();
        let sent = tokio::time::timeout(Duration::from_millis(300), t.lines.next_line()).await;
        assert!(sent.is_err(), "nothing sent to the task: {sent:?}");
        assert_eq!(fx.state("T1-x"), State::WaitingYou, "the card stays open in the task");
        assert!(!fx.said().iter().any(|l| l.contains("you answered")), "{:?}", fx.said());
    }

    #[tokio::test]
    async fn a_card_while_merging_keeps_merging() {
        let fx = fixture(5);
        let s = fx.task("T1-x", "faux/a", State::Merging);
        let mut t = fx.end(&s).await;
        t.catch_up(false, vec![], vec![ask(1)]).await;
        fx.until("asked", |fx| fx.asker.waiting.lock().unwrap().len() == 1)
            .await;
        assert_eq!(fx.state("T1-x"), State::Merging);
    }

    #[tokio::test]
    async fn turn_ends_on_the_socket_make_code_made_reports_that_wake_super() {
        let fx = fixture(5);
        let s = fx.task("T1-x", "faux/a", State::Running);
        let mut t = fx.end(&s).await;
        t.catch_up(true, vec![], vec![]).await;
        // The user's own chat in the task window: nothing.
        t.event(AgentEvent::AgentStart).await;
        t.event(end(&[None], StopReason::Stop)).await;
        // A progress report, then the turn on super's prompt ends.
        t.event(AgentEvent::AgentStart).await;
        t.event(report("progress")).await;
        t.event(says("the merge landed: abc123")).await;
        t.event(end(SUPER, StopReason::Stop)).await;
        fx.until("blocked", |fx| fx.state("T1-x") == State::Blocked).await;
        let said = fx.said();
        let wakes: Vec<_> = said.iter().filter(|l| l.starts_with("wake:")).collect();
        assert_eq!(wakes.len(), 1, "{said:?}");
        assert!(wakes[0].contains("T1-x is blocked (code-made report): ended its turn without a report"));
        assert!(
            wakes[0].contains("Its last message:\nthe merge landed: abc123"),
            "{}",
            wakes[0]
        );
        assert!(said.contains(&"desktop: T1-x blocked".to_string()));
    }

    #[tokio::test]
    async fn a_question_turn_wakes_super_with_the_reply_and_keeps_the_state() {
        let fx = fixture(5);
        let s = fx.task("T1-x", "faux/a", State::Ready);
        let mut t = fx.end(&s).await;
        t.catch_up(true, vec![], vec![]).await;
        t.event(AgentEvent::AgentStart).await;
        t.event(says("check 9 printed: ok")).await;
        t.event(end(QUESTION, StopReason::Stop)).await;
        fx.until("woken", |fx| fx.said().iter().any(|l| l.starts_with("wake:")))
            .await;
        let said = fx.said();
        assert!(
            said.contains(&"wake: T1-x replied: check 9 printed: ok".to_string()),
            "{said:?}"
        );
        assert_eq!(fx.state("T1-x"), State::Ready);
    }

    #[tokio::test]
    async fn a_turn_that_ended_while_disconnected_is_caught_up_on_reconnect() {
        let fx = fixture(5);
        let s = fx.task("T1-x", "faux/a", State::Running);
        let brief = "Task T1-x: the brief";
        fx.sup
            .edit(|b| b.tasks[0].last_prompt = Some(board::prompt_hash(brief)))
            .unwrap();
        let mut t = fx.end(&s).await;
        let history = vec![Message::user(brief), assistant(StopReason::Length, None)];
        t.catch_up(false, history, vec![]).await;
        fx.until("blocked", |fx| fx.state("T1-x") == State::Blocked).await;
        let block = fx.sup.row("T1-x").unwrap().block.unwrap();
        assert!(block.reason.contains("length limit"), "{block:?}");
    }

    #[tokio::test]
    async fn a_killed_task_fails_and_wakes_super_but_a_ready_one_stays_ready() {
        let fx = fixture(5);
        let s1 = fx.task("T1-x", "faux/a", State::Running);
        let s2 = fx.task("T2-y", "faux/a", State::Ready);
        let (mut t1, mut t2) = (fx.end(&s1).await, fx.end(&s2).await);
        t1.catch_up(true, vec![], vec![]).await;
        t2.catch_up(false, vec![], vec![]).await;
        fx.launcher.alive.lock().unwrap().clear();
        drop((t1, t2));
        fx.until("failed", |fx| fx.state("T1-x") == State::Failed).await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(fx.state("T2-y"), State::Ready);
        let wakes: Vec<_> = fx.said().into_iter().filter(|l| l.starts_with("wake:")).collect();
        assert_eq!(wakes, ["wake: T1-x failed: its process exited while it was working."]);
    }

    #[tokio::test]
    async fn a_quota_blocks_the_task_holds_only_its_provider_and_is_resent_when_it_clears() {
        let fx = fixture(5);
        let s = fx.task("T1-x", "claude/a", State::Running);
        let mut t = fx.end(&s).await;
        t.catch_up(true, vec![], vec![]).await;
        t.event(AgentEvent::AgentStart).await;
        let quota = StopReason::Error {
            kind: ErrorKind::QuotaExhausted,
        };
        t.event(end(SUPER, quota)).await;
        fx.until("blocked", |fx| fx.state("T1-x") == State::Blocked).await;
        let hold = fx.sup.board.lock().unwrap().holds.get("claude").cloned().unwrap();
        assert!(hold.reset_at.is_some_and(|r| r > board::now() + 800), "{hold:?}");
        assert!(
            fx.said()
                .iter()
                .filter(|l| l.starts_with("notice: Provider claude"))
                .count()
                == 1
        );

        // Queued work: the held provider waits, another starts.
        for (id, model) in [("T2-y", "claude/b"), ("T3-z", "codex/c")] {
            let t = Task {
                id: id.into(),
                label: id.into(),
                model: model.into(),
                state: State::Queued,
                request: "r".into(),
                ..Task::default()
            };
            fx.sup.edit(|b| b.tasks.push(t)).unwrap();
        }
        fx.links.kick();
        fx.until("T3 runs", |fx| fx.state("T3-z") == State::Running).await;
        assert_eq!(fx.state("T2-y"), State::Queued);

        // The limit clears: the hold lifts, T1 is resent to, T2 starts.
        fx.sup
            .edit(|b| b.holds.get_mut("claude").unwrap().reset_at = Some(board::now() - 1))
            .unwrap();
        fx.links.kick();
        fx.until("T1 running", |fx| fx.state("T1-x") == State::Running).await;
        fx.until("T2 runs", |fx| fx.state("T2-y") == State::Running).await;
        assert!(fx.sup.board.lock().unwrap().holds.is_empty());
        let sent = fx.launcher.sent.lock().unwrap().clone();
        assert!(
            sent.iter()
                .any(|(s2, c)| s2 == &s && matches!(c, ClientCmd::Prompt { text, raw: true } if text == LIMIT_CLEARED)),
            "{sent:?}"
        );
    }

    /// A repo with one commit on `main`.
    fn git_repo(dir: &std::path::Path) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join("f"), "x").unwrap();
        for args in [
            &["init", "-q", "-b", "main"][..],
            &["add", "."],
            &["commit", "-qm", "init"],
        ] {
            let ok = std::process::Command::new("git")
                .args([
                    "-c",
                    "user.name=t",
                    "-c",
                    "user.email=t@t",
                    "-c",
                    "commit.gpgsign=false",
                ])
                .args(args)
                .current_dir(dir)
                .output()
                .unwrap()
                .status
                .success();
            assert!(ok, "git {args:?}");
        }
    }

    impl Fx {
        /// `id` merging in `repo`, holding its merge lock.
        fn merging(&self, id: &str, repo: &std::path::Path) -> String {
            let s = self.task(id, "faux/a", State::Merging);
            self.sup
                .edit(|b| {
                    b.tasks.iter_mut().find(|t| t.id == id).unwrap().repo = Some(repo.to_path_buf());
                    b.merge_locks.insert(repo.to_path_buf(), id.into());
                })
                .unwrap();
            s
        }
        fn locks(&self) -> usize {
            self.sup.board.lock().unwrap().merge_locks.len()
        }
    }

    #[tokio::test]
    async fn a_killed_merging_task_releases_the_lock_and_a_half_done_merge_blocks_it() {
        let fx = fixture(5);
        let (clean, dirty) = (fx.sup.root.join("clean"), fx.sup.root.join("dirty"));
        git_repo(&clean);
        git_repo(&dirty);
        let s1 = fx.merging("T1-x", &clean);
        let s2 = fx.merging("T2-y", &dirty);
        let (mut t1, mut t2) = (fx.end(&s1).await, fx.end(&s2).await);
        t1.catch_up(true, vec![], vec![]).await;
        t2.catch_up(true, vec![], vec![]).await;
        std::fs::write(dirty.join("f"), "conflict markers").unwrap();
        fx.launcher.alive.lock().unwrap().clear();
        drop((t1, t2));
        fx.until("both out of merging", |fx| {
            fx.state("T1-x") != State::Merging && fx.state("T2-y") != State::Merging
        })
        .await;
        assert_eq!(fx.state("T1-x"), State::Failed);
        let t2 = fx.sup.row("T2-y").unwrap();
        assert_eq!(t2.state, State::Blocked);
        assert!(t2.block.unwrap().reason.contains("uncommitted changes"));
        assert_eq!(fx.locks(), 0, "both locks released");
        let wakes: Vec<_> = fx.said().into_iter().filter(|l| l.starts_with("wake:")).collect();
        assert!(
            wakes
                .iter()
                .any(|w| w.contains("T2-y is blocked") && w.contains("merge left half-done")),
            "{wakes:?}"
        );
        assert!(
            wakes
                .iter()
                .any(|w| w.contains("T1-x failed") && w.contains("merge lock is released")),
            "{wakes:?}"
        );
    }

    #[tokio::test]
    async fn merge_reports_release_the_lock_and_a_merged_task_gets_the_close_offer() {
        let fx = fixture(5);
        let (a, b) = (fx.sup.root.join("a"), fx.sup.root.join("b"));
        fx.merging("T1-x", &a);
        fx.merging("T2-y", &b);
        write_report(&fx, "T1-x", 1, "tok", json!({"merge": "merged", "summary": "ff"}));
        write_report(&fx, "T2-y", 1, "tok", json!({"merge": "pr-opened", "summary": "#12"}));
        fx.links.kick();
        fx.until("both ready", |fx| {
            fx.state("T1-x") == State::Ready && fx.state("T2-y") == State::Ready
        })
        .await;
        assert_eq!(fx.locks(), 0);
        assert_eq!(fx.sup.row("T2-y").unwrap().landed, Some(board::Landed::PrOpened));
        let wakes: Vec<_> = fx.said().into_iter().filter(|l| l.starts_with("wake:")).collect();
        assert!(
            wakes
                .iter()
                .any(|w| w.contains("T1-x merged") && w.contains("task close")),
            "{wakes:?}"
        );
        assert!(
            wakes.iter().any(|w| w.contains("T2-y opened a pull request")),
            "{wakes:?}"
        );
    }

    #[test]
    fn an_approved_non_git_write_task_gets_the_close_offer() {
        let mut t = Task {
            label: "T1-x".into(),
            write: true,
            state: State::Ready,
            ..Task::default()
        };
        let approved = Event::Verdict(board::Verdict::Approved);
        assert!(wake_text(&t, &approved).contains("task close"));
        t.repo = Some("/r".into());
        assert!(!wake_text(&t, &approved).contains("task close"));
    }

    #[tokio::test]
    async fn a_restart_links_live_tasks_marks_dead_ones_interrupted_restarts_dead_reviews_and_asks_once() {
        let fx = fixture_with(5, |sup, launcher| {
            let row = |id: &str, state: State, kind: Kind| Task {
                id: id.into(),
                label: id.into(),
                kind,
                model: "faux/a".into(),
                token: "tok".into(),
                state,
                session: Some(format!("s-{id}")),
                cwd: Some(sup.root.clone()),
                request: "r".into(),
                ..Task::default()
            };
            launcher.alive.lock().unwrap().insert("s-T1-live".into());
            let mut merging = row("T5-merge", State::Merging, Kind::Work);
            merging.repo = Some(sup.root.join("gone"));
            let mut reviewed = row("T6-rev", State::Reviewing, Kind::Work);
            (reviewed.write, reviewed.tier, reviewed.reviewer) =
                (true, Some("quick".into()), Some("T7-review-T6-rev".into()));
            let mut review = row("T7-review-T6-rev", State::Running, Kind::Review);
            review.reviewer_of = Some("T6-rev".into());
            sup.edit(|b| {
                b.tasks = vec![
                    row("T1-live", State::Running, Kind::Work),
                    row("T2-dead", State::Running, Kind::Work),
                    row("T3-wait", State::WaitingYou, Kind::Work),
                    row("T4-ready", State::Ready, Kind::Work),
                    merging,
                    reviewed,
                    review,
                ];
                b.merge_locks.insert(sup.root.join("gone"), "T5-merge".into());
            })
            .unwrap();
        });
        let mut live = fx.end("s-T1-live").await;
        live.catch_up(true, vec![], vec![]).await;
        assert_eq!(fx.state("T1-live"), State::Running);
        for id in ["T2-dead", "T3-wait", "T5-merge"] {
            assert_eq!(fx.state(id), State::Interrupted, "{id}");
        }
        assert_eq!(fx.state("T4-ready"), State::Ready, "not live: unchanged");
        assert_eq!(fx.locks(), 0, "the dead merge's lock is released");
        // The dead review is closed and a new one starts for the same task, without a round.
        assert_eq!(fx.state("T7-review-T6-rev"), State::Archived);
        let t6 = fx.sup.row("T6-rev").unwrap();
        assert_eq!((t6.state, t6.review_rounds), (State::Reviewing, 0));
        let new = t6.reviewer.clone().unwrap();
        assert!(new.starts_with("T8-review-T6-rev"), "{new}");
        fx.until("new review runs", |fx| fx.state(&new) == State::Running).await;

        // One card, one line per interrupted task.
        fx.until("asked", |fx| fx.asker.waiting.lock().unwrap().len() == 1)
            .await;
        let q = fx.asker.asked.lock().unwrap()[0].clone();
        let labels: Vec<&str> = q.options.iter().map(|o| o.label.as_str()).collect();
        assert_eq!(labels, ["T2-dead", "T3-wait", "T5-merge"]);
        assert!(q.multi);
        let tx = fx.asker.waiting.lock().unwrap().remove(0);
        let reply = Reply {
            question: q.question.clone(),
            chosen: vec!["T2-dead".into()],
            text: None,
            preview: None,
        };
        tx.send(Some(vec![reply])).unwrap();
        fx.until("T2 resumed", |fx| fx.state("T2-dead") == State::Running).await;
        assert_eq!(fx.state("T3-wait"), State::Interrupted);
        let starts = fx.launcher.starts.lock().unwrap().clone();
        assert!(
            starts.iter().any(|s| s.resume.as_deref() == Some("s-T2-dead")),
            "{starts:?}"
        );
        let sent = fx.launcher.sent.lock().unwrap().clone();
        assert!(
            sent.iter()
                .any(|(_, c)| matches!(c, ClientCmd::Prompt { text, raw: true } if text.contains("check your folder and git"))),
            "{sent:?}"
        );
        assert!(fx.asker.asked.lock().unwrap().len() == 1, "asked once");
    }

    fn write_report(fx: &Fx, task: &str, seq: u64, token: &str, fields: Value) {
        let dir = fx.sup.dir.join("inbox");
        std::fs::create_dir_all(&dir).unwrap();
        let mut r = json!({"id": format!("{task}-r{seq}"), "task": task, "seq": seq, "token": token});
        r.as_object_mut().unwrap().extend(fields.as_object().unwrap().clone());
        std::fs::write(dir.join(format!("{task}-{seq}.json")), r.to_string()).unwrap();
    }

    #[tokio::test]
    async fn inbox_reports_are_applied_and_wake_super_with_their_checks_and_bad_tokens_are_rejected() {
        let fx = fixture(5);
        fx.task("T1-x", "faux/a", State::Running);
        write_report(
            &fx,
            "T1-x",
            1,
            "tok",
            json!({"state": "progress", "summary": "halfway"}),
        );
        write_report(&fx, "T1-x", 2, "bad", json!({"state": "failed", "summary": "forged"}));
        write_report(
            &fx,
            "T1-x",
            3,
            "tok",
            json!({"state": "done", "summary": "the answer is 42",
                   "checks": [{"n": 1, "command": "cargo test", "exit_code": 0, "last_line": "ok"},
                              {"n": 0, "command": "sleep 45", "exit_code": 0, "last_line": "", "before_last_edit": true}]}),
        );
        fx.links.kick();
        fx.until("reviewing", |fx| fx.state("T1-x") == State::Reviewing).await;
        let row = fx.sup.row("T1-x").unwrap();
        assert_eq!(row.summary.as_deref(), Some("the answer is 42"));
        assert_eq!(row.applied, ["T1-x-r1", "T1-x-r3"]);
        assert!(fx.sup.dir.join("inbox/rejected/T1-x-2.json").exists());
        let wakes: Vec<_> = fx.said().into_iter().filter(|l| l.starts_with("wake:")).collect();
        assert_eq!(wakes.len(), 1, "progress doesn't wake: {wakes:?}");
        assert!(wakes[0].contains("T1-x reported done (now reviewing): the answer is 42"));
        assert!(wakes[0].contains("#1 `cargo test` → exit 0: ok"), "{}", wakes[0]);
        assert!(
            wakes[0].contains("#0 `sleep 45` → exit 0 (before the last edit)"),
            "{}",
            wakes[0]
        );
        let done = row.done.as_ref().unwrap();
        assert!(
            done.checks[1].ends_with("→ exit 0 (before the last edit)"),
            "{:?}",
            done.checks
        );
        // A read task: no review runs; super must judge it now.
        assert!(
            wakes[0].contains("No review runs for a read task") && wakes[0].contains("`task accept` or `task review`"),
            "{}",
            wakes[0]
        );
        let list = fx.sup.run(json!({"action": "list"})).await.unwrap();
        assert!(list.contains("| reviewing (your call) |"), "{list}");
    }

    #[tokio::test]
    async fn a_refused_done_report_leaves_the_recorded_done_facts_alone() {
        let fx = fixture(5);
        // `done` from `ready` is refused by the board.
        fx.task("T1-x", "faux/a", State::Ready);
        write_report(&fx, "T1-x", 1, "tok", json!({"state": "done", "summary": "late"}));
        let file = fx.sup.dir.join("inbox").join("T1-x-1.json");
        fx.links.kick();
        fx.until("applied", |_| !file.exists()).await;
        let row = fx.sup.row("T1-x").unwrap();
        assert_eq!((row.state, row.done.is_none()), (State::Ready, true));
    }

    #[tokio::test]
    async fn a_done_report_moves_a_task_on_from_a_code_made_block() {
        // R18: blocked by "goal done without a report", then the task's own `done` arrives.
        let fx = fixture(5);
        fx.task("T1-x", "faux/a", State::Running);
        fx.sup
            .edit(|b| b.apply("T1-x", Event::GoalEnded("goal done without a report".into())))
            .unwrap()
            .unwrap();
        write_report(&fx, "T1-x", 1, "tok", json!({"state": "done", "summary": "all good"}));
        fx.links.kick();
        fx.until("reviewing", |fx| fx.state("T1-x") == State::Reviewing).await;
        let row = fx.sup.row("T1-x").unwrap();
        assert_eq!((row.block, row.summary.as_deref()), (None, Some("all good")));
        let wakes: Vec<_> = fx.said().into_iter().filter(|l| l.starts_with("wake:")).collect();
        assert!(
            wakes
                .iter()
                .any(|w| w.contains("T1-x reported done (now reviewing): all good")),
            "{wakes:?}"
        );
    }
}
