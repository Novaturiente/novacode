//! The super board (L10.9 "Board", "States and transitions"): what super owns
//! about each task, the task state machine, and `board.json` / `board.md`.
//!
//! Only super's code writes the board, and only one super runs per root (its
//! lock), so `board.json` has one writer. The transition table lives in the
//! spec (`Progress/specs/super-mode.md`, "Task States"); [`next`] is that
//! table as a pure function, and [`Board::apply`] writes its result.

use novacode_base::storage::{Durability, write_atomic, write_atomic_no_backup};
use novacode_tui::panel::TaskRow;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum State {
    #[default]
    Queued,
    Running,
    WaitingYou,
    Blocked,
    Failed,
    Stopped,
    Interrupted,
    Reviewing,
    Ready,
    Merging,
    Archived,
}

impl State {
    /// Live states: the task's process is expected to be working.
    pub fn is_live(self) -> bool {
        matches!(self, State::Running | State::WaitingYou | State::Merging)
    }

    pub fn name(self) -> &'static str {
        match self {
            State::Queued => "queued",
            State::Running => "running",
            State::WaitingYou => "waiting-you",
            State::Blocked => "blocked",
            State::Failed => "failed",
            State::Stopped => "stopped",
            State::Interrupted => "interrupted",
            State::Reviewing => "reviewing",
            State::Ready => "ready",
            State::Merging => "merging",
            State::Archived => "archived",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Kind {
    #[default]
    Work,
    Review,
}

/// The brief's type: `question` is a read task, the others write.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TaskType {
    Bugfix,
    #[default]
    Change,
    Question,
}

impl TaskType {
    pub const ALL: [TaskType; 3] = [TaskType::Bugfix, TaskType::Change, TaskType::Question];

    /// Its serde name, as in a spawn's `type`.
    pub fn name(self) -> &'static str {
        match self {
            TaskType::Bugfix => "bugfix",
            TaskType::Change => "change",
            TaskType::Question => "question",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LimitKind {
    Quota,
    RateLimit,
}

impl LimitKind {
    /// In words, for block reasons and notes: `quota`, `rate limit`.
    pub fn label(self) -> &'static str {
        match self {
            LimitKind::Quota => "quota",
            LimitKind::RateLimit => "rate limit",
        }
    }
}

/// A provider limit that blocked a task or holds a provider.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Limit {
    pub kind: LimitKind,
    pub provider: String,
    /// Unix seconds, when the error carried one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reset_at: Option<u64>,
}

/// Why a task is `blocked`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Block {
    pub reason: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<Limit>,
    /// Made by code from a turn or goal end, not reported by the task: provisional,
    /// so the task's own terminal report still applies (R18).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub code_made: bool,
    /// Made by a failed merge or a merge exit: an approved task may merge again (R19).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub merge: bool,
}

impl Block {
    fn new(reason: impl Into<String>) -> Block {
        Block {
            reason: reason.into(),
            limit: None,
            code_made: false,
            merge: false,
        }
    }

    fn code_made(mut self) -> Block {
        self.code_made = true;
        self
    }

    /// A merge block is never provisional: the merge lock is gone, so no report or review
    /// moves the task on from it (only `task merge`, `send` or `close`).
    fn merge(mut self) -> Block {
        self.merge = true;
        self.code_made = false;
        self
    }
}

/// How a `ready` task's work landed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Landed {
    Merged,
    PrOpened,
}

/// What code recorded from a `done` report: the review judges exactly this.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Done {
    /// The reported (pinned) commit; `None` outside git.
    pub commit: Option<String>,
    pub files: Vec<String>,
    /// One entry per check: its recorded command, folder, exit code and whether it ran
    /// before the last edit, then the end of its recorded output when there is any.
    pub checks: Vec<String>,
    /// Screenshot paths and URLs.
    pub artifacts: Vec<String>,
    /// Built by code from the branch's commits: the task sent no report and no proof (R23).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub from_commits: bool,
}

/// A task's card super's code relayed, with its answer (L10.9 R25).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Asked {
    /// `question` or `approval`.
    pub kind: String,
    /// The question, or what the approval would run (cut to [`ASKED_TEXT`] chars).
    pub text: String,
    pub options: Vec<String>,
    pub answer: String,
    /// Unix seconds.
    pub at: u64,
}

/// Super's prompts to one session: questions and work never share a run (R24), so the
/// other kind waits here until the run that took the sent ones ends.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Flight {
    /// The session they went to.
    pub session: String,
    /// The kind of the sent ones: questions.
    pub question: bool,
    /// The prompt hashes sent and not yet seen consumed, oldest first.
    pub sent: Vec<String>,
    /// How many of `sent` were sent before the last run end seen: when the task then
    /// settles with nothing queued, no run took them (R27).
    pub ended: usize,
    /// Waiting, in order.
    pub held: Vec<Held>,
    /// How many of `sent` (the newest) the task has not confirmed yet: one may reach it only
    /// after a run ending now settled, so that settling doesn't count it (Y3).
    #[serde(skip)]
    pub unconfirmed: usize,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Held {
    pub question: bool,
    pub text: String,
}

/// Cards kept per row.
pub const ASKED_KEPT: usize = 10;
/// Chars of a card's text kept.
pub const ASKED_TEXT: usize = 200;

/// One board row: only what super owns. Live data (busy, current model,
/// tokens) comes from the task's run file and socket.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Task {
    pub id: String,
    pub label: String,
    pub kind: Kind,
    #[serde(rename = "type")]
    pub ty: TaskType,
    /// Relative to the root.
    pub folder: PathBuf,
    /// Main checkout of the task's repo; `None` outside git.
    pub repo: Option<PathBuf>,
    pub worktree: Option<PathBuf>,
    pub branch: Option<String>,
    pub base_branch: Option<String>,
    pub base_commit: Option<String>,
    pub tier: Option<String>,
    /// Model at spawn.
    pub model: String,
    pub trusted: bool,
    pub token: String,
    /// Super's spawn flag, or the task entered a linked worktree.
    pub write: bool,
    pub criteria: Vec<String>,
    pub scope: Option<String>,
    pub state: State,
    pub block: Option<Block>,
    /// Last report's summary.
    pub summary: Option<String>,
    pub question: Option<String>,
    pub review_rounds: u8,
    /// On a review task's row: the task it reviews.
    pub reviewer_of: Option<String>,
    /// On a reviewed row: the review task running now.
    pub reviewer: Option<String>,
    /// The running review uses no round (`task review` on a `ready` task).
    pub review_free: bool,
    /// Crashes of the current review (one automatic retry).
    pub review_crashes: u8,
    /// Ids of reports already applied (inbox dedupe and crash recovery).
    pub applied: Vec<String>,
    /// The facts of its last `done` report, for its review (never the task's summary).
    pub done: Option<Done>,
    /// The commit its last approved review covered: a later `done` on exactly that
    /// commit is ready without a new review (reviews are pinned to a commit).
    pub approved_commit: Option<String>,
    pub landed: Option<Landed>,
    /// Its last `task merge` opens a pull request; else code fast-forwards the main checkout
    /// after the task rebased (R29).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub pr: bool,
    /// A write task the user accepted on a card after its review rounds ran out.
    pub accepted: bool,
    /// The user's words, sent in the brief (kept so a queued task can start later).
    pub request: String,
    /// Skills the brief names.
    pub skills: Vec<String>,
    /// `task spawn {goal: true}`: an open-ended task.
    pub goal: bool,
    /// Messages sent while `queued`: added to the brief.
    pub pending: Vec<String>,
    /// Follow-ups super sent with `task send` (not code's resends): the review brief carries them.
    pub follow_ups: Vec<String>,
    /// Its relayed cards and their answers, as code saw them (the last [`ASKED_KEPT`]): the review brief carries them.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub asked: Vec<Asked>,
    /// The task's session id, once its process started.
    pub session: Option<String>,
    /// The folder its process runs in (absolute; in its worktree for a write task).
    pub cwd: Option<PathBuf>,
    /// Hash of super's last prompt to it ([`prompt_hash`]): the catch-up after a
    /// reconnect finds it in the task's messages.
    pub last_prompt: Option<String>,
    /// Super's prompts to its session in flight and held (R24); saved so a restart keeps them (R27).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub flight: Option<Flight>,
    /// Session tokens last seen in its run file: the Tasks tab shows them once it's gone.
    pub tokens: u64,
    /// Unix seconds.
    pub started: u64,
    pub updated: u64,
}

impl Task {
    /// Record a relayed card's answer, keeping the last [`ASKED_KEPT`].
    pub fn record_asked(&mut self, a: Asked) {
        self.asked.push(a);
        let extra = self.asked.len().saturating_sub(ASKED_KEPT);
        self.asked.drain(..extra);
    }

    /// The state with its block reason or marks: `blocked: quota`, `ready (merged)`,
    /// `reviewing (your call)` for a read task no review task judges.
    pub fn state_text(&self) -> String {
        let state = self.state.name();
        if let Some(b) = &self.block {
            return format!("{state}: {}", b.reason);
        }
        let mut marks = Vec::new();
        if self.state == State::Reviewing && !self.write && self.reviewer.is_none() {
            marks.push("your call");
        }
        if self.accepted {
            marks.push("accepted without review");
        }
        match self.landed {
            Some(Landed::Merged) => marks.push("merged"),
            Some(Landed::PrOpened) => marks.push("PR opened"),
            None => {}
        }
        match marks.is_empty() {
            true => state.to_string(),
            false => format!("{state} ({})", marks.join(", ")),
        }
    }

    /// Its last `done` is on the commit an approved review already covered.
    pub fn approval_covers_done(&self) -> bool {
        let done = self.done.as_ref().and_then(|d| d.commit.as_ref());
        done.is_some() && done == self.approved_commit.as_ref()
    }

    /// The repo's folder name, else the task's folder; `.` for the root itself.
    pub fn repo_name(&self) -> String {
        let name = match self.repo.as_deref().and_then(Path::file_name) {
            Some(n) => n.to_string_lossy().into_owned(),
            None => self.folder.display().to_string(),
        };
        if name.is_empty() { ".".into() } else { name }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReportState {
    Progress,
    Done,
    Blocked,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Approved,
    NeedsProof,
    Rejected,
}

impl Verdict {
    pub const ALL: [Verdict; 3] = [Verdict::Approved, Verdict::NeedsProof, Verdict::Rejected];

    /// Its name in a review report's `verdict`.
    pub fn name(self) -> &'static str {
        match self {
            Verdict::Approved => "approved",
            Verdict::NeedsProof => "needs-proof",
            Verdict::Rejected => "rejected",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MergeResult {
    Merged,
    PrOpened,
    Failed(String),
}

/// Everything that can move a task, one per row group of the spec's table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// A slot frees and this queued task is the one picked.
    SlotFree,
    CardOpened,
    LastCardAnswered,
    Report {
        state: ReportState,
        summary: String,
        question: Option<String>,
    },
    /// A turn on super's prompt ended with no terminal report.
    TurnUnreported,
    /// A turn on super's prompt ended on an error or the length limit.
    TurnError(String),
    /// A turn ended on a quota or rate limit.
    TurnLimit(Limit),
    /// The provider limit cleared; code resends.
    LimitCleared,
    /// An active goal ended without a terminal report (names the goal's end).
    GoalEnded(String),
    ProcessExit,
    /// `task accept`.
    Accept,
    /// `task review`.
    Review,
    ReviewCantStart(String),
    Verdict(Verdict),
    BranchMoved,
    ReviewCrashed(String),
    /// `task merge`, after the user confirmed on the card.
    Merge,
    MergeReport(MergeResult),
    /// `task send`.
    Send,
    /// `task stop`.
    Stop,
    /// Super restarted and found the process dead.
    Restart,
    /// `task close`, after the user confirmed on the card.
    Close,
    /// Review task row: its verdict or crash was applied.
    Retire,
}

/// Work the caller does after a transition; the state is already decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Effect {
    StartReview {
        uses_round: bool,
    },
    StopReview,
    RelayCard,
    /// The task left `merging` other than by a merge report: the lock is
    /// released; check the main checkout and call [`Board::merge_left`] if
    /// the merge was left half-done.
    CheckMergeLeft,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Next {
    pub from: State,
    pub to: State,
    pub block: Option<Block>,
    pub effects: Vec<Effect>,
}

/// The block reason once a write task's review rounds are used up.
pub const ROUNDS_USED_UP: &str = "review rounds used up";

/// The transition table. `lock_holder` is the task holding the merge lock of
/// this task's repo, if another one does. `Err` is the refusal message.
pub fn next(t: &Task, ev: &Event, lock_holder: Option<&str>) -> Result<Next, String> {
    use State::*;
    let from = t.state;
    let go = |to: State| {
        Ok(Next {
            from,
            to,
            block: None,
            effects: vec![],
        })
    };
    let with = |to: State, effect: Effect| {
        Ok(Next {
            from,
            to,
            block: None,
            effects: vec![effect],
        })
    };
    let blocked = |block: Block| {
        Ok(Next {
            from,
            to: Blocked,
            block: Some(block),
            effects: vec![],
        })
    };
    let refused = || {
        Err(format!(
            "{}: {} is not possible while it is {}",
            t.label,
            event_name(ev),
            from.name()
        ))
    };
    let stop_review = |to: State| match t.reviewer {
        Some(_) => with(to, Effect::StopReview),
        None => go(to),
    };

    if t.kind == Kind::Review {
        // A review task's own row; everything else goes through the reviewed task.
        return match (ev, from) {
            (Event::SlotFree, Queued) => go(Running),
            (Event::CardOpened, Running | WaitingYou) => go(WaitingYou),
            (Event::LastCardAnswered, WaitingYou) => go(Running),
            (Event::Retire, s) if s != Archived => go(Archived),
            (Event::SlotFree | Event::CardOpened | Event::LastCardAnswered | Event::Retire, _) => refused(),
            _ => Err(format!(
                "{} is a review task; act on the reviewed task {} instead",
                t.label,
                t.reviewer_of.as_deref().unwrap_or("?")
            )),
        };
    }

    // Every way out of `merging` other than a merge report (merge exits).
    if from == Merging {
        let to = match ev {
            Event::Stop => Some(Stopped),
            // A goal that ended unreported is a turn end too (R24: it used to be refused, leaving the row stuck).
            Event::TurnUnreported | Event::TurnError(_) | Event::TurnLimit(_) | Event::GoalEnded(_) => Some(Blocked),
            Event::ProcessExit => Some(Failed),
            Event::Restart => Some(Interrupted),
            _ => None,
        };
        if let Some(to) = to {
            let mut n = with(to, Effect::CheckMergeLeft)?;
            if to == Blocked {
                n.block = Some(turn_block(ev).merge());
            }
            return Ok(n);
        }
    }

    let reviewing = t.reviewer.is_some();
    let block_is = |f: fn(&Block) -> bool| t.block.as_ref().is_some_and(f);
    // A code-made block is provisional: the task's own terminal report applies as from `running`.
    let from_report = match from {
        Blocked if t.block.as_ref().is_some_and(|b| b.code_made) => Running,
        s => s,
    };
    match (ev, from) {
        (Event::SlotFree, Queued) => go(Running),

        (Event::CardOpened, Running | WaitingYou) => go(WaitingYou),
        (Event::CardOpened, Merging | Reviewing) => with(from, Effect::RelayCard),
        (Event::LastCardAnswered, WaitingYou) => go(Running),

        (Event::Report { state, .. }, _) => match (state, from_report) {
            (ReportState::Progress, _) if from != from_report => refused(),
            (ReportState::Progress, s) if s.is_live() => go(s),
            (ReportState::Blocked, Running) => blocked(Block::new("reported blocked")),
            (ReportState::Failed, Running) => go(Failed),
            // Reviews are pinned to a commit: the approval of this exact commit still holds.
            (ReportState::Done, Running) if t.write && t.approval_covers_done() => go(Ready),
            (ReportState::Done, Running) if t.write => with(Reviewing, Effect::StartReview { uses_round: true }),
            (ReportState::Done, Running) => go(Reviewing),
            // A new commit while its review runs: that review judged old work (the caller
            // refuses a `done` on the commit under review).
            (ReportState::Done, Reviewing) if t.write && reviewing && t.approval_covers_done() => stop_review(Ready),
            (ReportState::Done, Reviewing) if t.write && reviewing => Ok(Next {
                from,
                to: Reviewing,
                block: None,
                effects: vec![
                    Effect::StopReview,
                    Effect::StartReview {
                        uses_round: !t.review_free,
                    },
                ],
            }),
            _ => refused(),
        },

        (Event::TurnUnreported | Event::TurnError(_) | Event::TurnLimit(_), Running) => blocked(turn_block(ev)),
        // A merge block is never resent: its merge lock is gone (`task merge` retries it, R19).
        (Event::LimitCleared, Blocked) if block_is(|b| b.limit.is_some() && !b.merge) => go(Running),
        (Event::GoalEnded(how), Running) => blocked(Block::new(how.clone()).code_made()),

        (Event::ProcessExit, Running | WaitingYou) => go(Failed),
        (Event::ProcessExit | Event::Restart, s) if !s.is_live() => go(s),
        (Event::Restart, Running | WaitingYou) => go(Interrupted),

        (Event::Accept, Reviewing) if !t.write => stop_review(Ready),
        // A read task that answered in text without `report` (R20).
        (Event::Accept, Blocked) if !t.write && block_is(|b| b.code_made) => go(Ready),
        (Event::Accept, Reviewing) => Err(format!(
            "{}: a write task becomes ready only through an approved review",
            t.label
        )),
        // The user's call once the rounds are used up (the caller asks on a card first).
        (Event::Accept, Blocked) if t.write && t.block.as_ref().is_some_and(|b| b.reason == ROUNDS_USED_UP) => {
            go(Ready)
        }
        (Event::Review, Reviewing) if !t.write && !reviewing => {
            with(Reviewing, Effect::StartReview { uses_round: true })
        }
        // A write task that keeps ending without a report: the caller records its commits first (R23);
        // a head already approved needs no new review (R27).
        (Event::Review, Blocked) if t.write && block_is(|b| b.code_made) && t.approval_covers_done() => go(Ready),
        (Event::Review, Blocked) if t.write && block_is(|b| b.code_made) => {
            with(Reviewing, Effect::StartReview { uses_round: true })
        }
        (Event::Review, Ready) if t.landed != Some(Landed::Merged) => {
            with(Reviewing, Effect::StartReview { uses_round: false })
        }
        (Event::ReviewCantStart(why), Reviewing) => blocked(Block::new(why.clone())),

        (Event::Verdict(v), Reviewing) if reviewing => match v {
            Verdict::Approved => go(Ready),
            Verdict::NeedsProof | Verdict::Rejected => {
                let rounds = t.review_rounds + u8::from(!t.review_free);
                if rounds < 2 {
                    go(Running)
                } else {
                    blocked(Block::new(ROUNDS_USED_UP))
                }
            }
        },
        (Event::BranchMoved, Reviewing) if reviewing => go(Running),
        (Event::ReviewCrashed(why), Reviewing) if reviewing => {
            if t.review_crashes == 0 {
                with(
                    Reviewing,
                    Effect::StartReview {
                        uses_round: !t.review_free,
                    },
                )
            } else {
                blocked(Block::new(format!("review task crashed twice: {why}")))
            }
        }

        // Retry a failed merge while the approval still covers the done commit (R19).
        (Event::Merge, Ready | Blocked)
            if from == Ready || block_is(|b| b.merge) && (t.approval_covers_done() || t.accepted) =>
        {
            if t.repo.is_none() {
                Err(format!(
                    "{} is not in a git repo; nothing to merge: ask the user whether to close it (`task close`)",
                    t.label
                ))
            } else if !t.write || t.branch.is_none() {
                Err(format!(
                    "{} has no branch of its own (a read task): nothing to merge",
                    t.label
                ))
            } else if t.landed == Some(Landed::Merged) {
                Err(format!("{} is already merged", t.label))
            } else if let Some(other) = lock_holder {
                Err(format!("{other} is merging in this repo; try again after it"))
            } else {
                go(Merging)
            }
        }
        (Event::MergeReport(m), Merging) => match m {
            MergeResult::Merged | MergeResult::PrOpened => go(Ready),
            MergeResult::Failed(why) => blocked(Block::new(why.clone()).merge()),
        },

        (Event::Send, Blocked | Failed | Stopped | Ready | Interrupted) => go(Running),
        (Event::Send, Reviewing) => stop_review(Running),
        (Event::Send, Running | WaitingYou | Merging | Queued) => go(from),
        (Event::Stop, Running | WaitingYou | Queued) => go(Stopped),
        (Event::Stop, Reviewing) => stop_review(Stopped),
        (Event::Stop, Blocked) => Err(format!(
            "{} is not running (blocked): `task close` ends it, `task send` gives it more work",
            t.label
        )),
        (Event::Close, s) if !matches!(s, Archived | Merging) => stop_review(Archived),

        _ => refused(),
    }
}

/// The block reason of a turn that ended without a terminal report.
fn turn_block(ev: &Event) -> Block {
    match ev {
        Event::TurnError(text) | Event::GoalEnded(text) => Block::new(text.clone()),
        Event::TurnLimit(limit) => Block {
            reason: limit.kind.label().into(),
            limit: Some(limit.clone()),
            code_made: false,
            merge: false,
        },
        _ => Block::new("ended its turn without a report"),
    }
    .code_made()
}

fn event_name(ev: &Event) -> &'static str {
    match ev {
        Event::SlotFree => "starting from the queue",
        Event::CardOpened => "a card",
        Event::LastCardAnswered => "an answered card",
        Event::Report { .. } => "this report",
        Event::TurnUnreported | Event::TurnError(_) | Event::TurnLimit(_) => "a turn end",
        Event::LimitCleared => "a limit clearing",
        Event::GoalEnded(_) => "a goal end",
        Event::ProcessExit => "a process exit",
        Event::Accept => "accept",
        Event::Review => "review",
        Event::ReviewCantStart(_) => "a review start error",
        Event::Verdict(_) => "a verdict",
        Event::BranchMoved => "a branch move",
        Event::ReviewCrashed(_) => "a review crash",
        Event::Merge => "merge",
        Event::MergeReport(_) => "a merge report",
        Event::Send => "send",
        Event::Stop => "stop",
        Event::Restart => "a restart",
        Event::Close => "close",
        Event::Retire => "retire",
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Board {
    pub tasks: Vec<Task>,
    /// Repo main checkout → task id holding its merge lock.
    pub merge_locks: BTreeMap<PathBuf, String>,
    /// Provider → the limit holding it.
    pub holds: BTreeMap<String, Limit>,
}

impl Board {
    pub fn task(&self, id: &str) -> Option<&Task> {
        self.tasks.iter().find(|t| t.id == id)
    }

    /// Run `ev` through [`next`] and write the result to the row and the
    /// board-wide merge locks and provider holds. Nothing is saved.
    pub fn apply(&mut self, id: &str, ev: Event) -> Result<Next, String> {
        let i = self
            .tasks
            .iter()
            .position(|t| t.id == id)
            .ok_or_else(|| format!("no task {id}"))?;
        let holder = self.tasks[i]
            .repo
            .as_ref()
            .and_then(|repo| self.merge_locks.get(repo))
            .filter(|holder| **holder != self.tasks[i].id)
            .map(|holder| self.task(holder).map_or(holder.as_str(), |h| &h.label));
        let n = next(&self.tasks[i], &ev, holder)?;

        let t = &mut self.tasks[i];
        t.state = n.to;
        // A report replaces a provisional block even when the state stays `blocked`.
        if n.from != n.to || matches!(ev, Event::Report { .. }) && n.block.is_some() {
            t.block = n.block.clone();
            if n.to != State::Blocked {
                t.question = None;
            }
        }
        if n.to == State::Running {
            t.landed = None;
            t.accepted = false;
        }
        if ev == Event::Accept && n.from == State::Blocked && t.write {
            t.accepted = true;
        }
        let ends_review = matches!(
            ev,
            Event::Verdict(_) | Event::BranchMoved | Event::ReviewCrashed(_) | Event::ReviewCantStart(_)
        ) || n.effects.contains(&Effect::StopReview);
        if ends_review {
            t.reviewer = None;
        }
        for effect in &n.effects {
            if let Effect::StartReview { uses_round } = *effect {
                t.review_free = !uses_round;
                if !matches!(ev, Event::ReviewCrashed(_)) {
                    t.review_crashes = 0;
                }
            }
        }
        let mut release = n.effects.contains(&Effect::CheckMergeLeft);
        match &ev {
            Event::Report { summary, question, .. } => {
                t.summary = Some(summary.clone());
                t.question = question.clone();
            }
            Event::ReviewCrashed(_) => t.review_crashes += 1,
            Event::Verdict(v) => {
                if *v != Verdict::Approved && !t.review_free {
                    t.review_rounds += 1;
                }
                if *v == Verdict::Approved {
                    t.approved_commit = t.done.as_ref().and_then(|d| d.commit.clone());
                }
                t.review_free = false;
                t.review_crashes = 0;
            }
            Event::MergeReport(m) => {
                release = true;
                t.landed = match m {
                    MergeResult::Merged => Some(Landed::Merged),
                    MergeResult::PrOpened => Some(Landed::PrOpened),
                    MergeResult::Failed(_) => None,
                };
            }
            Event::TurnLimit(limit) => {
                self.holds.insert(limit.provider.clone(), limit.clone());
            }
            _ => {}
        }
        let t = &mut self.tasks[i];
        t.updated = now();
        if let Some(repo) = t.repo.clone() {
            if ev == Event::Merge {
                self.merge_locks.insert(repo, t.id.clone());
            } else if release && self.merge_locks.get(&repo) == Some(&t.id) {
                self.merge_locks.remove(&repo);
            }
        }
        Ok(n)
    }

    /// A merge exit found the main checkout half-merged: block the task.
    pub fn merge_left(&mut self, id: &str, reason: &str) -> Result<(), String> {
        let t = self
            .tasks
            .iter_mut()
            .find(|t| t.id == id)
            .ok_or_else(|| format!("no task {id}"))?;
        t.state = State::Blocked;
        t.block = Some(Block::new(reason).merge());
        t.updated = now();
        Ok(())
    }

    /// The provider's limit cleared: its queued tasks may start again.
    pub fn lift_hold(&mut self, provider: &str) {
        self.holds.remove(provider);
    }

    /// `<dir>/board.json`; a missing file is an empty board.
    pub fn load(dir: &Path) -> io::Result<Board> {
        // A corrupt board.json is restored from its `.bak` when that parses.
        Ok(novacode_base::storage::read_json(&dir.join("board.json"))?.unwrap_or_default())
    }

    /// Write `board.json` atomically, then the readable `board.md`.
    pub fn save(&self, dir: &Path) -> io::Result<()> {
        let json = serde_json::to_vec_pretty(self).map_err(io::Error::other)?;
        write_atomic(&dir.join("board.json"), &json, Durability::Durable)?;
        write_atomic_no_backup(&dir.join("board.md"), self.render().as_bytes(), Durability::Fast)
    }

    /// Save the tokens `live(session)` reads from each task's run file on its row.
    /// Returns whether a row changed (the board then needs saving).
    pub fn note_tokens(&mut self, live: impl Fn(&str) -> Option<(String, u64)>) -> bool {
        let mut changed = false;
        for t in &mut self.tasks {
            if let Some((_, n)) = t.session.as_deref().and_then(&live)
                && n != t.tokens
            {
                t.tokens = n;
                changed = true;
            }
        }
        changed
    }

    /// The board snapshot for the Tasks tab: every task not archived, with the
    /// live model and tokens `live(session)` reads from its run file, else the
    /// model at spawn and the tokens last seen.
    pub fn snapshot(&self, live: impl Fn(&str) -> Option<(String, u64)>) -> Vec<TaskRow> {
        self.tasks
            .iter()
            .filter(|t| t.state != State::Archived)
            .map(|t| {
                let (model, tokens) = t
                    .session
                    .as_deref()
                    .and_then(&live)
                    .unwrap_or_else(|| (t.model.clone(), t.tokens));
                TaskRow {
                    label: t.label.clone(),
                    repo: t.repo_name(),
                    state: t.state_text(),
                    model,
                    tokens,
                    started_ms: t.started * 1000,
                    summary: t.summary.clone(),
                    session: t.session.clone(),
                }
            })
            .collect()
    }

    /// The board as a markdown table, archived rows last.
    pub fn render(&self) -> String {
        let cell = |s: &str| s.replace('|', "\\|").replace('\n', " ");
        let mut out = String::from(
            "# Super board\n\n| Task | Repo | Kind | State | Model | Summary |\n|---|---|---|---|---|---|\n",
        );
        let (live, archived): (Vec<_>, Vec<_>) = self.tasks.iter().partition(|t| t.state != State::Archived);
        for t in live.into_iter().chain(archived) {
            let kind = match t.kind {
                Kind::Work => "work",
                Kind::Review => "review",
            };
            out.push_str(&format!(
                "| {} | {} | {kind} | {} | {} | {} |\n",
                cell(&t.label),
                cell(&t.repo_name()),
                cell(&t.state_text()),
                cell(&t.model),
                cell(t.summary.as_deref().unwrap_or("")),
            ));
        }
        if !self.merge_locks.is_empty() || !self.holds.is_empty() {
            out.push('\n');
        }
        for (repo, id) in &self.merge_locks {
            out.push_str(&format!("- Merge lock: {} held by {id}\n", repo.display()));
        }
        for (provider, limit) in &self.holds {
            out.push_str(&format!("- On hold: {provider} ({})\n", limit.kind.label()));
        }
        out
    }
}

/// A stable hash of a prompt's text (FNV-1a, hex): the same in every super process.
pub fn prompt_hash(text: &str) -> String {
    novacode_core::trust::fnv_hex(text.as_bytes())
}

/// Unix seconds: the board's clock.
pub fn now() -> u64 {
    novacode_core::session::now_ms() / 1000
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(state: State) -> Task {
        Task {
            id: "T1".into(),
            label: "T1-fix".into(),
            state,
            write: true,
            repo: Some("/r".into()),
            branch: Some("worktree-T1-fix".into()),
            ..Task::default()
        }
    }

    fn report(state: ReportState) -> Event {
        Event::Report {
            state,
            summary: "s".into(),
            question: None,
        }
    }

    fn limit() -> Limit {
        Limit {
            kind: LimitKind::Quota,
            provider: "claude".into(),
            reset_at: None,
        }
    }

    fn to(t: &Task, ev: Event) -> State {
        next(t, &ev, None)
            .unwrap_or_else(|e| panic!("{ev:?} from {:?} refused: {e}", t.state))
            .to
    }

    fn effects(t: &Task, ev: Event) -> Vec<Effect> {
        next(t, &ev, None).unwrap().effects
    }

    use State::*;
    const ALL: [State; 11] = [
        Queued,
        Running,
        WaitingYou,
        Blocked,
        Failed,
        Stopped,
        Interrupted,
        Reviewing,
        Ready,
        Merging,
        Archived,
    ];
    const LIVE: [State; 3] = [Running, WaitingYou, Merging];

    #[test]
    fn spawn_rows() {
        assert_eq!(to(&row(Queued), Event::SlotFree), Running);
    }

    #[test]
    fn card_rows() {
        assert_eq!(to(&row(Running), Event::CardOpened), WaitingYou);
        for s in [Merging, Reviewing] {
            let n = next(&row(s), &Event::CardOpened, None).unwrap();
            assert_eq!(n.to, s);
            assert_eq!(n.effects, vec![Effect::RelayCard]);
        }
        assert_eq!(to(&row(WaitingYou), Event::LastCardAnswered), Running);
    }

    #[test]
    fn report_rows() {
        for s in LIVE {
            assert_eq!(to(&row(s), report(ReportState::Progress)), s);
        }
        assert_eq!(to(&row(Running), report(ReportState::Blocked)), Blocked);
        assert_eq!(to(&row(Running), report(ReportState::Failed)), Failed);
        // Done: a read task goes to super's judgement, a write task to review.
        let read = Task {
            write: false,
            ..row(Running)
        };
        let n = next(&read, &report(ReportState::Done), None).unwrap();
        assert_eq!((n.to, n.effects), (Reviewing, vec![]));
        let n = next(&row(Running), &report(ReportState::Done), None).unwrap();
        assert_eq!(
            (n.to, n.effects),
            (Reviewing, vec![Effect::StartReview { uses_round: true }])
        );
        // Done again on the commit an approved review covered: ready, no new review.
        let done_on = |c: &str| Done {
            commit: Some(c.into()),
            ..Done::default()
        };
        let covered = Task {
            done: Some(done_on("c1")),
            approved_commit: Some("c1".into()),
            ..row(Running)
        };
        let n = next(&covered, &report(ReportState::Done), None).unwrap();
        assert_eq!((n.to, n.effects), (Ready, vec![]));
        let other = Task {
            done: Some(done_on("c2")),
            ..covered
        };
        assert_eq!(to(&other, report(ReportState::Done)), Reviewing);
    }

    #[test]
    fn a_task_report_overrides_a_code_made_block() {
        // R18: code-made blocks are provisional; the task's own terminal report applies.
        for ev in [
            Event::TurnUnreported,
            Event::TurnError("e".into()),
            Event::TurnLimit(limit()),
            Event::GoalEnded("goal done without a report".into()),
        ] {
            let mut b = Board {
                tasks: vec![row(Running)],
                ..Board::default()
            };
            b.apply("T1", ev.clone()).unwrap();
            assert_eq!(b.task("T1").unwrap().state, Blocked);
            let n = b.apply("T1", report(ReportState::Done)).unwrap();
            assert_eq!(
                (n.from, n.to, n.effects),
                (Blocked, Reviewing, vec![Effect::StartReview { uses_round: true }]),
                "{ev:?}"
            );
            assert_eq!(b.task("T1").unwrap().block, None);
            let mut b2 = Board {
                tasks: vec![row(Running)],
                ..Board::default()
            };
            b2.apply("T1", ev.clone()).unwrap();
            b2.apply("T1", report(ReportState::Blocked)).unwrap();
            assert_eq!(b2.task("T1").unwrap().block, Some(Block::new("reported blocked")));
            b2.apply("T1", Event::Send).unwrap();
            b2.apply("T1", Event::TurnUnreported).unwrap();
            assert_eq!(to(b2.task("T1").unwrap(), report(ReportState::Failed)), Failed);
        }
        // A block the task reported itself, or one from review, is not provisional.
        let mut b = Board {
            tasks: vec![row(Running)],
            ..Board::default()
        };
        b.apply("T1", report(ReportState::Blocked)).unwrap();
        assert!(b.apply("T1", report(ReportState::Done)).is_err());
        let used_up = Task {
            block: Some(Block::new(ROUNDS_USED_UP)),
            ..row(Blocked)
        };
        assert!(next(&used_up, &report(ReportState::Done), None).is_err());
    }

    #[test]
    fn an_approved_verdict_records_its_commit() {
        let mut b = Board {
            tasks: vec![Task {
                id: "T1".into(),
                done: Some(Done {
                    commit: Some("c1".into()),
                    ..Done::default()
                }),
                ..under_review(0)
            }],
            ..Board::default()
        };
        b.apply("T1", Event::Verdict(Verdict::Approved)).unwrap();
        assert_eq!(b.task("T1").unwrap().approved_commit.as_deref(), Some("c1"));
    }

    #[test]
    fn turn_end_rows() {
        let n = next(&row(Running), &Event::TurnUnreported, None).unwrap();
        assert_eq!(n.to, Blocked);
        assert!(n.block.unwrap().reason.contains("without a report"));
        let n = next(&row(Running), &Event::TurnError("overloaded".into()), None).unwrap();
        assert_eq!((n.to, n.block.unwrap().reason), (Blocked, "overloaded".into()));
        let n = next(&row(Running), &Event::TurnLimit(limit()), None).unwrap();
        assert_eq!((n.to, n.block.unwrap().limit), (Blocked, Some(limit())));
        let held = Task {
            block: Some(Block {
                reason: "quota".into(),
                limit: Some(limit()),
                code_made: true,
                merge: false,
            }),
            ..row(Blocked)
        };
        assert_eq!(to(&held, Event::LimitCleared), Running);
        // A plain block doesn't clear with the limit.
        assert!(next(&row(Blocked), &Event::LimitCleared, None).is_err());
        let n = next(&row(Running), &Event::GoalEnded("goal done".into()), None).unwrap();
        assert_eq!((n.to, n.block.unwrap().reason), (Blocked, "goal done".into()));
    }

    #[test]
    fn process_exit_and_restart_rows() {
        for s in ALL {
            let exit = to(&row(s), Event::ProcessExit);
            let restart = to(&row(s), Event::Restart);
            match s {
                Running | WaitingYou => assert_eq!((exit, restart), (Failed, Interrupted)),
                Merging => assert_eq!((exit, restart), (Failed, Interrupted)),
                // Not live: the state stays.
                _ => assert_eq!((exit, restart), (s, s), "{s:?}"),
            }
        }
    }

    #[test]
    fn read_task_review_rows() {
        let read = Task {
            write: false,
            ..row(Reviewing)
        };
        assert_eq!(to(&read, Event::Accept), Ready);
        let n = next(&read, &Event::Review, None).unwrap();
        assert_eq!(
            (n.to, n.effects),
            (Reviewing, vec![Effect::StartReview { uses_round: true }])
        );
        // A write task becomes ready only through an approved verdict.
        assert!(next(&row(Reviewing), &Event::Accept, None).is_err());
    }

    #[test]
    fn a_write_task_out_of_review_rounds_can_be_accepted_and_is_marked() {
        let used_up = Task {
            block: Some(Block::new(ROUNDS_USED_UP)),
            ..row(Blocked)
        };
        assert_eq!(to(&used_up, Event::Accept), Ready);
        assert!(next(&row(Blocked), &Event::Accept, None).is_err(), "other blocks");
        let mut b = Board {
            tasks: vec![Task {
                id: "T1".into(),
                label: "T1-x".into(),
                ..used_up
            }],
            ..Board::default()
        };
        b.apply("T1", Event::Accept).unwrap();
        let t = b.task("T1").unwrap();
        assert!(t.accepted);
        assert_eq!(t.state_text(), "ready (accepted without review)");
        assert!(b.render().contains("ready (accepted without review)"));
        b.apply("T1", Event::Send).unwrap();
        assert!(!b.task("T1").unwrap().accepted, "cleared when it works again");
    }

    #[test]
    fn a_read_task_blocked_by_code_can_be_accepted() {
        // R20: a read task answered in text without `report`; super judges the answer.
        let read = |state| Task {
            write: false,
            ..row(state)
        };
        let mut b = Board {
            tasks: vec![read(Running)],
            ..Board::default()
        };
        b.apply("T1", Event::TurnUnreported).unwrap();
        b.apply("T1", Event::Accept).unwrap();
        let t = b.task("T1").unwrap();
        assert_eq!((t.state, t.state_text()), (Ready, "ready".into()));
        // A block the task reported, or a write task's code-made block, still refuses accept.
        let mut b = Board {
            tasks: vec![
                read(Running),
                Task {
                    id: "T2".into(),
                    ..row(Running)
                },
            ],
            ..Board::default()
        };
        b.apply("T1", report(ReportState::Blocked)).unwrap();
        assert!(b.apply("T1", Event::Accept).is_err(), "reported block");
        b.apply("T2", Event::TurnUnreported).unwrap();
        assert!(b.apply("T2", Event::Accept).is_err(), "write task");
    }

    #[test]
    fn a_failed_merge_of_an_approved_task_can_be_retried() {
        // R19: the merge failed on a dirty main checkout; once it is clean, merge again.
        let approved = |id: &str, done: &str| Task {
            id: id.into(),
            label: format!("{id}-x"),
            repo: Some(format!("/r{id}").into()),
            done: Some(Done {
                commit: Some(done.into()),
                ..Done::default()
            }),
            approved_commit: Some("c1".into()),
            ..row(Ready)
        };
        let mut b = Board {
            tasks: vec![approved("T1", "c1"), approved("T2", "c1"), approved("T3", "c2")],
            ..Board::default()
        };
        let failed = Event::MergeReport(MergeResult::Failed("main checkout has uncommitted changes".into()));
        for id in ["T1", "T2", "T3"] {
            b.apply(id, Event::Merge).unwrap();
        }
        b.apply("T1", failed.clone()).unwrap();
        b.apply("T1", Event::Merge).unwrap();
        assert_eq!(b.task("T1").unwrap().state, Merging);
        assert_eq!(b.merge_locks.get(Path::new("/rT1")).map(String::as_str), Some("T1"));
        // A merge exit's block (a turn end while merging) is a merge block too.
        b.apply("T2", Event::TurnUnreported).unwrap();
        b.apply("T2", Event::Merge).unwrap();
        // Not when the done commit is not the approved one.
        b.apply("T3", failed).unwrap();
        assert!(b.apply("T3", Event::Merge).is_err(), "unapproved commit");
        // A task the user accepted without review may retry too.
        b.tasks.iter_mut().find(|t| t.id == "T3").unwrap().accepted = true;
        b.apply("T3", Event::Merge).unwrap();
        // Other blocks still refuse merge.
        let other = Task {
            block: Some(Block::new("reported blocked")),
            ..approved("T4", "c1")
        };
        let other = Task {
            state: Blocked,
            ..other
        };
        assert!(next(&other, &Event::Merge, None).is_err(), "other block");
    }

    #[test]
    fn a_read_task_in_reviewing_without_a_review_is_super_s_call() {
        let read = Task {
            write: false,
            ..row(Reviewing)
        };
        assert_eq!(read.state_text(), "reviewing (your call)");
        let reviewed = Task {
            reviewer: Some("T2".into()),
            ..read
        };
        assert_eq!(reviewed.state_text(), "reviewing");
        assert_eq!(under_review(0).state_text(), "reviewing");
    }

    fn under_review(rounds: u8) -> Task {
        Task {
            reviewer: Some("T2".into()),
            review_rounds: rounds,
            ..row(Reviewing)
        }
    }

    #[test]
    fn review_rows() {
        let n = next(&row(Reviewing), &Event::ReviewCantStart("denied".into()), None).unwrap();
        assert_eq!((n.to, n.block.unwrap().reason), (Blocked, "denied".into()));
        assert_eq!(to(&under_review(0), Event::Verdict(Verdict::Approved)), Ready);
        // A doubted approval: back to reviewing, no round used.
        let n = next(&row(Ready), &Event::Review, None).unwrap();
        assert_eq!(
            (n.to, n.effects),
            (Reviewing, vec![Effect::StartReview { uses_round: false }])
        );
        let merged = Task {
            landed: Some(Landed::Merged),
            ..row(Ready)
        };
        assert!(next(&merged, &Event::Review, None).is_err());
        // Rounds counted after this verdict: the first sends back, the second blocks.
        for v in [Verdict::NeedsProof, Verdict::Rejected] {
            assert_eq!(to(&under_review(0), Event::Verdict(v)), Running);
            assert_eq!(to(&under_review(1), Event::Verdict(v)), Blocked);
        }
        assert_eq!(to(&under_review(0), Event::BranchMoved), Running);
        let n = next(&under_review(0), &Event::ReviewCrashed("oom".into()), None).unwrap();
        assert_eq!(
            (n.to, n.effects),
            (Reviewing, vec![Effect::StartReview { uses_round: true }])
        );
        let again = Task {
            review_crashes: 1,
            ..under_review(0)
        };
        let n = next(&again, &Event::ReviewCrashed("oom".into()), None).unwrap();
        assert_eq!(n.to, Blocked);
        assert!(n.block.unwrap().reason.contains("oom"));
        // Review-only rows need a running review task.
        assert!(next(&row(Reviewing), &Event::BranchMoved, None).is_err());
        assert!(next(&row(Reviewing), &Event::Verdict(Verdict::Approved), None).is_err());
    }

    #[test]
    fn merge_rows() {
        assert_eq!(to(&row(Ready), Event::Merge), Merging);
        let e = next(&row(Ready), &Event::Merge, Some("T9-other")).unwrap_err();
        assert!(e.contains("T9-other"), "{e}");
        let nogit = Task {
            repo: None,
            ..row(Ready)
        };
        assert!(next(&nogit, &Event::Merge, None).is_err());
        assert!(next(&row(Reviewing), &Event::Merge, None).is_err());
        let r = |m| Event::MergeReport(m);
        assert_eq!(to(&row(Merging), r(MergeResult::Merged)), Ready);
        assert_eq!(to(&row(Merging), r(MergeResult::PrOpened)), Ready);
        let n = next(&row(Merging), &r(MergeResult::Failed("conflict".into())), None).unwrap();
        assert_eq!((n.to, n.block.unwrap().reason), (Blocked, "conflict".into()));
    }

    #[test]
    fn a_limit_block_made_while_merging_is_not_resent_when_the_limit_clears() {
        // A3 (audit r5): the merge lock is gone; a resend would finish the merge without it.
        let mut b = board(&[("T1", Merging)]);
        b.apply("T1", Event::TurnLimit(limit())).unwrap();
        let block = b.task("T1").unwrap().block.clone().unwrap();
        assert!(block.merge && block.limit.is_some(), "{block:?}");
        assert!(b.apply("T1", Event::LimitCleared).is_err());
        assert_eq!(b.task("T1").unwrap().state, Blocked);
    }

    #[test]
    fn a_done_on_a_new_commit_while_reviewing_restarts_the_review() {
        // A5 (audit r5): the review judged the old commit.
        let mut b = Board {
            tasks: vec![Task {
                id: "T1".into(),
                ..under_review(0)
            }],
            ..Board::default()
        };
        let n = b.apply("T1", report(ReportState::Done)).unwrap();
        assert_eq!(
            (n.to, n.effects),
            (
                Reviewing,
                vec![Effect::StopReview, Effect::StartReview { uses_round: true }]
            )
        );
        assert_eq!(b.task("T1").unwrap().reviewer, None, "the old review is retired");
        // A read task super judges itself still refuses a second `done`.
        let read = Task {
            write: false,
            ..row(Reviewing)
        };
        assert!(next(&read, &report(ReportState::Done), None).is_err());
    }

    #[test]
    fn accepting_a_read_task_under_review_stops_the_review() {
        // A6 (audit r5).
        let read = Task {
            write: false,
            ..under_review(0)
        };
        let n = next(&read, &Event::Accept, None).unwrap();
        assert_eq!((n.to, n.effects), (Ready, vec![Effect::StopReview]));
    }

    #[test]
    fn a_crashed_free_review_is_retried_free() {
        // A7 (audit r5): `task review` on a ready task uses no round, nor does its retry.
        let mut b = Board {
            tasks: vec![Task {
                id: "T1".into(),
                review_free: true,
                ..under_review(1)
            }],
            ..Board::default()
        };
        let n = b.apply("T1", Event::ReviewCrashed("oom".into())).unwrap();
        assert_eq!(n.effects, vec![Effect::StartReview { uses_round: false }]);
        assert!(b.task("T1").unwrap().review_free);
    }

    #[test]
    fn a_free_review_restarted_by_a_new_done_stays_free() {
        // L10.9 Y5: `task review` on a ready task uses no round, nor does its restart.
        let mut b = Board {
            tasks: vec![Task {
                id: "T1".into(),
                review_free: true,
                ..under_review(1)
            }],
            ..Board::default()
        };
        let n = b.apply("T1", report(ReportState::Done)).unwrap();
        assert_eq!(
            n.effects,
            vec![Effect::StopReview, Effect::StartReview { uses_round: false }]
        );
        assert!(b.task("T1").unwrap().review_free);
    }

    #[test]
    fn close_is_refused_while_merging() {
        // A11 (audit r5): the table itself refuses it, not only `task close`.
        assert!(next(&row(Merging), &Event::Close, None).is_err());
    }

    #[test]
    fn merge_needs_a_write_task_with_a_branch() {
        // A12 (audit r5): a ready read task in a git repo has nothing to merge.
        let read = Task {
            write: false,
            ..row(Ready)
        };
        let e = next(&read, &Event::Merge, None).unwrap_err();
        assert!(e.contains("nothing to merge"), "{e}");
        let no_branch = Task {
            branch: None,
            ..row(Ready)
        };
        assert!(next(&no_branch, &Event::Merge, None).is_err());
    }

    #[test]
    fn merge_exit_rows() {
        let cases = [
            (Event::Stop, Stopped),
            (Event::TurnUnreported, Blocked),
            (Event::TurnError("e".into()), Blocked),
            (Event::GoalEnded("goal done without a report".into()), Blocked),
            (Event::ProcessExit, Failed),
            (Event::Restart, Interrupted),
        ];
        for (ev, want) in cases {
            let n = next(&row(Merging), &ev, None).unwrap();
            assert_eq!((n.to, n.effects), (want, vec![Effect::CheckMergeLeft]), "{ev:?}");
        }
    }

    #[test]
    fn send_stop_close_rows() {
        for s in [Blocked, Failed, Stopped, Reviewing, Ready, Interrupted] {
            assert_eq!(to(&row(s), Event::Send), Running, "{s:?}");
        }
        assert_eq!(effects(&under_review(0), Event::Send), vec![Effect::StopReview]);
        for s in [Running, WaitingYou, Merging, Queued] {
            assert_eq!(to(&row(s), Event::Send), s, "{s:?}");
        }
        for s in [Running, WaitingYou, Queued, Reviewing] {
            assert_eq!(to(&row(s), Event::Stop), Stopped, "{s:?}");
        }
        assert_eq!(effects(&under_review(0), Event::Stop), vec![Effect::StopReview]);
        for s in ALL.into_iter().filter(|s| !matches!(s, Archived | Merging)) {
            assert_eq!(to(&row(s), Event::Close), Archived, "{s:?}");
        }
    }

    #[test]
    fn stop_on_a_blocked_task_hints_at_close_and_send() {
        // R23: a blocked task is not running; stop points at what ends it or moves it on.
        let e = next(&row(Blocked), &Event::Stop, None).unwrap_err();
        assert_eq!(
            e,
            "T1-fix is not running (blocked): `task close` ends it, `task send` gives it more work"
        );
    }

    #[test]
    fn review_from_a_code_made_block_uses_a_round_but_not_from_a_reported_one() {
        // R23: a write task that keeps ending without a report can be reviewed from its commits.
        let mut b = Board {
            tasks: vec![row(Running)],
            ..Board::default()
        };
        b.apply("T1", Event::TurnUnreported).unwrap();
        let n = b.apply("T1", Event::Review).unwrap();
        assert_eq!(
            (n.from, n.to, n.effects),
            (Blocked, Reviewing, vec![Effect::StartReview { uses_round: true }])
        );
        assert_eq!(b.task("T1").unwrap().block, None);
        let reported = Task {
            block: Some(Block::new("reported blocked")),
            ..row(Blocked)
        };
        let e = next(&reported, &Event::Review, None).unwrap_err();
        assert!(e.contains("review is not possible while it is blocked"), "{e}");
    }

    #[test]
    fn a_merge_exit_block_is_not_provisional() {
        // L10.9 Y1: a turn end while merging blocks the task, but no report or review moves it
        // on from there: only `task merge` (R19), `task send` or `task close`.
        for accepted in [false, true] {
            let merging = Task {
                done: Some(Done {
                    commit: Some("c1".into()),
                    ..Done::default()
                }),
                approved_commit: (!accepted).then(|| "c1".into()),
                accepted,
                ..row(Merging)
            };
            let mut b = Board {
                tasks: vec![merging],
                ..Board::default()
            };
            b.apply("T1", Event::TurnUnreported).unwrap();
            assert!(b.task("T1").unwrap().block.as_ref().unwrap().merge);
            for ev in [report(ReportState::Done), report(ReportState::Failed), Event::Review] {
                assert!(b.apply("T1", ev.clone()).is_err(), "{ev:?} accepted={accepted}");
            }
            assert_eq!(b.task("T1").unwrap().accepted, accepted);
            b.apply("T1", Event::Merge).unwrap();
        }
    }

    #[test]
    fn refused_pairs() {
        let refused = [
            (Archived, Event::Send),
            (Archived, Event::Close),
            (Ready, Event::Stop),
            (Blocked, Event::Stop),
            (Queued, Event::Merge),
            (Running, Event::Accept),
            (Blocked, report(ReportState::Done)),
            (Ready, report(ReportState::Progress)),
            (Queued, Event::CardOpened),
            (Running, Event::LastCardAnswered),
            (Reviewing, Event::TurnUnreported),
            (Ready, Event::MergeReport(MergeResult::Merged)),
            (Running, Event::Retire),
        ];
        for (s, ev) in refused {
            let e = next(&row(s), &ev, None).expect_err(&format!("{ev:?} from {s:?}"));
            assert!(!e.is_empty());
        }
    }

    fn board(rows: &[(&str, State)]) -> Board {
        Board {
            tasks: rows
                .iter()
                .map(|(id, s)| Task {
                    id: id.to_string(),
                    label: format!("{id}-x"),
                    ..row(*s)
                })
                .collect(),
            ..Board::default()
        }
    }

    #[test]
    fn merge_lock_is_one_per_repo_and_released_on_every_exit() {
        let mut b = board(&[("T1", Ready), ("T2", Ready)]);
        b.apply("T1", Event::Merge).unwrap();
        let e = b.apply("T2", Event::Merge).unwrap_err();
        assert!(e.contains("T1-x"), "{e}");
        b.apply("T1", Event::MergeReport(MergeResult::PrOpened)).unwrap();
        assert_eq!(b.task("T1").unwrap().landed, Some(Landed::PrOpened));
        b.apply("T2", Event::Merge).unwrap();
        b.apply("T2", Event::Stop).unwrap();
        assert!(b.merge_locks.is_empty());
        // A half-done merge blocks the task with the reason.
        b.merge_left("T2", "rebase in progress").unwrap();
        let t = b.task("T2").unwrap();
        assert_eq!(
            (t.state, t.block.clone().unwrap().reason),
            (Blocked, "rebase in progress".into())
        );
    }

    #[test]
    fn apply_keeps_rounds_holds_and_reports() {
        let mut b = board(&[("T1", Running)]);
        b.apply(
            "T1",
            Event::Report {
                state: ReportState::Blocked,
                summary: "need a key".into(),
                question: Some("which key?".into()),
            },
        )
        .unwrap();
        let t = b.task("T1").unwrap();
        assert_eq!(t.summary.as_deref(), Some("need a key"));
        assert_eq!(t.question.as_deref(), Some("which key?"));
        b.apply("T1", Event::Send).unwrap();
        let t = b.task("T1").unwrap();
        assert_eq!((t.state, &t.block, &t.question), (Running, &None, &None));

        b.apply("T1", Event::TurnLimit(limit())).unwrap();
        assert_eq!(b.holds.get("claude"), Some(&limit()));
        b.lift_hold("claude");
        assert!(b.holds.is_empty());
        b.apply("T1", Event::LimitCleared).unwrap();

        // Two non-approving verdicts use up the rounds.
        for want in [Running, Blocked] {
            b.apply("T1", report(ReportState::Done)).unwrap();
            b.tasks[0].reviewer = Some("T2".into());
            b.apply("T1", Event::Verdict(Verdict::Rejected)).unwrap();
            assert_eq!(b.task("T1").unwrap().state, want);
            b.apply("T1", Event::Send).unwrap();
        }
        // A doubted approval's review uses no round.
        b.tasks[0].state = Ready;
        b.apply("T1", Event::Review).unwrap();
        b.tasks[0].reviewer = Some("T3".into());
        b.apply("T1", Event::Verdict(Verdict::Approved)).unwrap();
        assert_eq!(b.task("T1").unwrap().review_rounds, 2);
        assert!(b.apply("T9", Event::Send).is_err());
    }

    #[test]
    fn save_and_load_round_trip() {
        let dir = std::env::temp_dir().join(format!("nc-board-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(Board::load(&dir).unwrap(), Board::default());
        let mut b = board(&[("T1", Ready), ("T2", Running)]);
        b.apply("T1", Event::Merge).unwrap();
        b.save(&dir).unwrap();
        assert_eq!(Board::load(&dir).unwrap(), b);
        let md = std::fs::read_to_string(dir.join("board.md")).unwrap();
        assert!(md.contains("| T1-x |") && md.contains("merging"), "{md}");
        let json = std::fs::read_to_string(dir.join("board.json")).unwrap();
        assert!(json.contains("\"waiting-you\"") || json.contains("\"running\""));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// C8: a corrupt board.json is restored from its backup instead of stopping super.
    #[test]
    fn a_corrupt_board_loads_from_its_backup() {
        let d = tempfile::tempdir().unwrap();
        let first = board(&[("T1", Ready)]);
        first.save(d.path()).unwrap();
        board(&[("T1", Ready), ("T2", Running)]).save(d.path()).unwrap();
        std::fs::write(d.path().join("board.json"), "{\"tasks\": [").unwrap();
        assert_eq!(Board::load(d.path()).unwrap(), first);
    }

    #[test]
    fn board_md_render() {
        let b = Board {
            tasks: vec![
                Task {
                    id: "T2".into(),
                    label: "T2-old".into(),
                    state: Archived,
                    model: "claude/claude-haiku-5-5".into(),
                    ..Task::default()
                },
                Task {
                    id: "T1".into(),
                    label: "T1-fix-parser".into(),
                    folder: "proj/sub".into(),
                    repo: Some("/root/proj".into()),
                    state: Blocked,
                    block: Some(Block::new("needs a | pipe")),
                    model: "claude/claude-sonnet-5-5".into(),
                    summary: Some("two lines\nof summary".into()),
                    ..Task::default()
                },
                Task {
                    id: "T3".into(),
                    label: "T3-review-T1-fix-parser".into(),
                    kind: Kind::Review,
                    folder: "notes".into(),
                    state: Ready,
                    landed: Some(Landed::Merged),
                    model: "codex/gpt-6.1-sol:high".into(),
                    ..Task::default()
                },
            ],
            merge_locks: BTreeMap::from([(PathBuf::from("/root/proj"), "T1".to_string())]),
            holds: BTreeMap::from([("claude".to_string(), limit())]),
        };
        let want = "# Super board

| Task | Repo | Kind | State | Model | Summary |
|---|---|---|---|---|---|
| T1-fix-parser | proj | work | blocked: needs a \\| pipe | claude/claude-sonnet-5-5 | two lines of summary |
| T3-review-T1-fix-parser | notes | review | ready (merged) | codex/gpt-6.1-sol:high |  |
| T2-old | . | work | archived | claude/claude-haiku-5-5 |  |

- Merge lock: /root/proj held by T1
- On hold: claude (quota)
";
        assert_eq!(b.render(), want);
    }

    #[test]
    fn snapshot_rows_take_live_data_and_skip_archived() {
        let mut b = board(&[("T1", Running), ("T2", Archived), ("T3", Queued)]);
        b.tasks[0].session = Some("s1".into());
        b.tasks[0].started = 100;
        b.tasks[0].summary = Some("half done".into());
        b.tasks[2].model = "claude/claude-haiku-5-5".into();
        let rows = b.snapshot(|s| (s == "s1").then(|| ("codex/gpt-6.1-sol".to_string(), 1234)));
        let got: Vec<_> = rows
            .iter()
            .map(|r| (r.label.as_str(), r.state.as_str(), r.model.as_str(), r.tokens))
            .collect();
        assert_eq!(
            got,
            [
                ("T1-x", "running", "codex/gpt-6.1-sol", 1234),
                ("T3-x", "queued", "claude/claude-haiku-5-5", 0)
            ]
        );
        assert_eq!(rows[0].started_ms, 100_000);
        assert_eq!(rows[0].session.as_deref(), Some("s1"));
        assert_eq!(rows[0].summary.as_deref(), Some("half done"));
        assert_eq!(rows[0].repo, "r");
    }

    #[test]
    fn a_task_s_last_seen_tokens_stay_after_its_run_file_is_gone() {
        let mut b = board(&[("T1", Running)]);
        b.tasks[0].session = Some("s1".into());
        let live = |s: &str| (s == "s1").then(|| ("faux/a".to_string(), 1234));
        assert!(b.note_tokens(live));
        assert!(!b.note_tokens(live), "unchanged: nothing to save");
        b.tasks[0].state = Stopped;
        let rows = b.snapshot(|_| None);
        assert_eq!(rows[0].tokens, 1234);
        let back: Board = serde_json::from_slice(&serde_json::to_vec(&b).unwrap()).unwrap();
        assert_eq!(back.tasks[0].tokens, 1234, "saved on the row");
    }

    #[test]
    fn review_task_row_lifecycle() {
        let rev = |s| Task {
            kind: Kind::Review,
            write: false,
            reviewer_of: Some("T1".into()),
            ..row(s)
        };
        assert_eq!(to(&rev(Queued), Event::SlotFree), Running);
        assert_eq!(to(&rev(Running), Event::CardOpened), WaitingYou);
        assert_eq!(to(&rev(WaitingYou), Event::LastCardAnswered), Running);
        assert_eq!(to(&rev(Running), Event::Retire), Archived);
        assert_eq!(to(&rev(Queued), Event::Retire), Archived);
        for ev in [Event::Send, Event::Stop, Event::Close, Event::Merge, Event::Review] {
            let e = next(&rev(Running), &ev, None).unwrap_err();
            assert!(e.contains("T1"), "{e}");
        }
    }
}
