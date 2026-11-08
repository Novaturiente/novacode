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
    /// Unix seconds the hold started (its last limit hit); none on an old board's hold,
    /// which counts as started long ago.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since: Option<u64>,
}

/// The longest a rate limit holds its provider, whatever its reset time says: a task
/// that hits it again holds the provider again.
pub const RATE_LIMIT_HOLD_MAX: u64 = 60 * 60;

impl Limit {
    /// When the hold lifts by time: the reset time, and for a rate limit at most
    /// [`RATE_LIMIT_HOLD_MAX`] after it started.
    pub fn lifts_at(&self) -> Option<u64> {
        match self.kind {
            LimitKind::Quota => self.reset_at,
            LimitKind::RateLimit => {
                let cap = self.since.unwrap_or(0) + RATE_LIMIT_HOLD_MAX;
                Some(self.reset_at.map_or(cap, |r| r.min(cap)))
            }
        }
    }
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

/// Where a task's after-merge steps stand.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AfterState {
    #[default]
    NotRun,
    /// Sent to the task once its merge landed; its next `done` needs no review.
    Running,
    Done,
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

/// What super's code saw of a task's work, for super's health checks (L10.9 "Health
/// checks and progress"): written from the task's socket events, shown in the state block.
/// Unix seconds; 0 = never.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Health {
    /// The task's last socket event.
    pub last_event: u64,
    /// The tool running longest now, and since when.
    pub tool: Option<String>,
    pub tool_since: u64,
    /// Tool calls in a row that ended in an error.
    pub errors: u32,
    /// How many times in a row the same tool call (name and arguments) ran.
    pub repeats: u32,
    /// Relayed cards not answered yet, and when the oldest opened.
    pub cards: u32,
    pub card_since: u64,
    /// Its last `progress` report.
    pub progress_at: u64,
}

/// `t` seconds before `now`, short: `40 s`, `12 min`, `3 h`.
pub(super) fn age(now: u64, t: u64) -> String {
    let s = now.saturating_sub(t);
    match s {
        0..120 => format!("{s} s"),
        120..7200 => format!("{} min", s / 60),
        _ => format!("{} h", s / 3600),
    }
}

impl Health {
    /// One line of facts for the state block.
    pub fn line(&self, now: u64) -> String {
        let mut parts = vec![match self.last_event {
            0 => "no event seen yet".to_string(),
            t => format!("last event {} ago", age(now, t)),
        }];
        parts.push(match &self.tool {
            Some(tool) => format!("running `{tool}` for {}", age(now, self.tool_since)),
            None => "no tool running".into(),
        });
        if self.errors > 0 {
            parts.push(format!("{} tool errors in a row", self.errors));
        }
        if self.repeats > 1 {
            parts.push(format!("the same call {} times in a row", self.repeats));
        }
        if self.cards > 0 {
            let s = if self.cards == 1 { "" } else { "s" };
            parts.push(format!(
                "{} unanswered card{s}, oldest {}",
                self.cards,
                age(now, self.card_since)
            ));
        }
        parts.push(match self.progress_at {
            0 => "no progress report yet".into(),
            t => format!("last progress {} ago", age(now, t)),
        });
        parts.join("; ")
    }
}

/// Chars of a report summary in a status line.
pub const STATUS_SUMMARY: usize = 80;

/// The first `N%` in `s`.
fn percent(s: &str) -> Option<String> {
    s.split(|c: char| !(c.is_ascii_digit() || c == '%'))
        .find(|w| w.len() > 1 && w.ends_with('%') && w[..w.len() - 1].chars().all(|c| c.is_ascii_digit()))
        .map(String::from)
}

/// An open question or warning super sent the user: the Needs you list (L10.9).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Need {
    /// The `say` call's id.
    pub id: String,
    /// `question` or `warning`.
    pub kind: String,
    pub tasks: Vec<String>,
    pub status: String,
    /// Unix seconds.
    pub at: u64,
    /// A question with no card (one exact answer): super's next `reply` answers it.
    pub typed: bool,
    /// A question's card, kept so a restarted super opens it again.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub card: Option<novacode_types::Question>,
}

/// Chars of a task's charter notes before the oldest are summarised.
pub const NOTES_CAP: usize = 3000;
/// Chars of a summary row in the state block.
const STATE_SUMMARY: usize = 400;

/// Cards kept per row.
pub const ASKED_KEPT: usize = 10;
/// Chars of a card's text kept.
pub const ASKED_TEXT: usize = 200;

/// A merge train (L10.9): its first task cherry-picks every task's commits, in order, onto
/// `branch`, which lands as one merge.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Train {
    pub branch: String,
    /// The other tasks, in merge order.
    pub tasks: Vec<String>,
}

/// The refusal of super's question or message about a task the user paused.
pub fn paused(name: &str) -> String {
    format!("the user paused {name}; only the user resumes it")
}

/// A task held on purpose (L10.9 `task hold`): stall checks and health nags skip it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Hold {
    pub reason: String,
    /// The user paused it: super may not ask about it or message it; only `task resume`
    /// (when the user asked) ends it. Else super's own hold, which ends when the task
    /// reports or super sends it work.
    pub user: bool,
    /// Unix seconds.
    pub at: u64,
}

/// One board row: only what super owns. Live data (busy, current model,
/// tokens) comes from the task's run file and socket.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Task {
    pub id: String,
    /// `<sid4>-T<n>-<slug>` (an older board's row: `T<n>-<slug>`); shown by [`name`](Self::name).
    pub label: String,
    /// Last 4 characters of the super session that spawned it; empty on an older board's row.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub sid: String,
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
    /// Steps that run only once the merge landed (deploy, live checks): never judged by its review.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub after: Vec<String>,
    /// Where its [`after`](Self::after) steps stand.
    #[serde(skip_serializing_if = "is_default")]
    pub after_state: AfterState,
    /// Short points: the areas the change should touch (an old board's string is one point).
    #[serde(deserialize_with = "points", skip_serializing_if = "Vec::is_empty")]
    pub scope: Vec<String>,
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
    /// The charter's decisions (L10.9 "Super's context over days"), written only by code:
    /// relayed card answers and super's `task note`s, oldest first. Over [`NOTES_CAP`]
    /// chars the oldest are summarised into one note, never dropped silently.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
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
    /// What code saw of its work, for super's health checks.
    #[serde(skip_serializing_if = "is_default")]
    pub health: Health,
    /// Unix seconds of its last report of any state; 0 = none yet.
    #[serde(skip_serializing_if = "is_default")]
    pub report_at: u64,
    /// Unix seconds of super's last action on it (send, review, merge, accept, scope, note,
    /// close, a `say` question naming it): a task with none for long is stalled.
    #[serde(skip_serializing_if = "is_default")]
    pub acted_at: u64,
    /// The user chose on the deploy card (or dismissed it): it isn't asked again.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub deploy_decided: bool,
    /// Code's deploy card is open now (not saved: a restart has no open card).
    #[serde(skip)]
    pub asking: bool,
    /// On a merge train's first task: the integration branch and the other tasks in it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub train: Option<Train>,
    /// The user approved its merge on a train's card and the train failed: its next
    /// `task merge` needs no card.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub merge_approved: bool,
    /// `task hold`: the user paused it, or super keeps it waiting on purpose (L10.9).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hold: Option<Hold>,
    /// Unix seconds.
    pub started: u64,
    pub updated: u64,
}

impl Task {
    /// Its label without its session: `T<n>-<slug>`.
    pub fn short(&self) -> &str {
        self.label
            .strip_prefix(&self.sid)
            .and_then(|r| r.strip_prefix('-'))
            .filter(|_| !self.sid.is_empty())
            .unwrap_or(&self.label)
    }

    /// `T<n>` of its label.
    pub fn number(&self) -> u64 {
        super::task::number(self.short())
    }

    /// How super and the panel show it while super session `cur` runs: `T1-slug` for that
    /// session's tasks, `<sid4>·T16-slug` for another's, `old·T16-slug` for an older board's row.
    pub fn name(&self, cur: &str) -> String {
        match (self.sid.as_str(), self.sid == cur) {
            (_, true) => self.short().to_string(),
            ("", false) => format!("old·{}", self.label),
            (sid, false) => format!("{sid}·{}", self.short()),
        }
    }

    /// Record a relayed card's answer, keeping the last [`ASKED_KEPT`].
    pub fn record_asked(&mut self, a: Asked) {
        self.asked.push(a);
        let extra = self.asked.len().saturating_sub(ASKED_KEPT);
        self.asked.drain(..extra);
    }

    /// Chars of its charter notes.
    #[cfg(test)]
    pub fn notes_len(&self) -> usize {
        self.notes.iter().map(String::len).sum()
    }

    /// The charter: the brief and criteria word for word from spawn (or a scope card), the
    /// scope, and the decisions recorded since.
    pub fn charter(&self) -> String {
        let mut s = format!("### {} charter\nBrief: {}\n", self.label, self.request.trim());
        if !self.criteria.is_empty() {
            s.push_str("Criteria:\n");
            for (i, c) in self.criteria.iter().enumerate() {
                s.push_str(&format!("{}. {c}\n", i + 1));
            }
        }
        if !self.after.is_empty() {
            s.push_str("After merge (run only once the merge lands; not judged by the review):\n");
            for (i, c) in self.after.iter().enumerate() {
                s.push_str(&format!("{}. {c}\n", i + 1));
            }
        }
        if !self.scope.is_empty() {
            s.push_str("Scope:\n");
            for p in &self.scope {
                s.push_str(&format!("- {p}\n"));
            }
        }
        if !self.notes.is_empty() {
            s.push_str("Decisions and notes:\n");
            for n in &self.notes {
                s.push_str(&format!("- {n}\n"));
            }
        }
        s
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
        match self.after_state {
            AfterState::Running => marks.push("after steps"),
            AfterState::Done => marks.push("after steps done"),
            AfterState::NotRun => {}
        }
        match marks.is_empty() {
            true => state.to_string(),
            false => format!("{state} ({})", marks.join(", ")),
        }
    }

    /// One status line for the user (`say` blocks, code-made cards): label, state, a
    /// percentage from the last report, its summary cut short, and the report's age.
    pub fn status_line(&self, now: u64) -> String {
        let mut parts = vec![self.label.clone(), self.state_text()];
        if let Some(s) = self
            .summary
            .as_deref()
            .map(|s| s.split_whitespace().collect::<Vec<_>>().join(" "))
        {
            if let Some(p) = percent(&s) {
                parts.push(p);
            }
            let mut short: String = s.chars().take(STATUS_SUMMARY).collect();
            if s.chars().count() > STATUS_SUMMARY {
                short.push('…');
            }
            parts.push(short);
        }
        parts.push(match self.report_at {
            0 => "no report yet".into(),
            t => format!("last report {} ago", age(now, t)),
        });
        parts.join(" · ")
    }

    /// The charter note, scope point, brief or criterion that holds back its after steps,
    /// such as "stop before deploy" or "no push": code then asks the user on a card before
    /// sending them.
    pub fn after_limit(&self) -> Option<&str> {
        const LIMITS: [&str; 9] = [
            "no push",
            "stop before push",
            "stop before",
            "before deploy",
            "don't deploy",
            "do not deploy",
            "no deploy",
            "not deploy",
            "without deploy",
        ];
        self.notes
            .iter()
            .map(String::as_str)
            .chain(self.scope.iter().map(String::as_str))
            .chain(std::iter::once(self.request.as_str()))
            .chain(self.criteria.iter().map(String::as_str))
            .find(|n| {
                let n = n.to_lowercase().replace('’', "'");
                LIMITS.iter().any(|l| n.contains(l))
            })
    }

    /// `held: <reason>`, or `paused by you: <reason>` for the user's hold.
    pub fn hold_text(&self) -> Option<String> {
        self.hold.as_ref().map(|h| match h.user {
            true => format!("paused by you: {}", h.reason),
            false => format!("held: {}", h.reason),
        })
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
    /// Code sends a merged task its after-merge steps.
    AfterSteps,
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
            // Its after-merge steps are not reviewed: the review judged the merged work.
            (ReportState::Done, Running)
                if t.after_state == AfterState::Running
                    && (!t.write || t.done.as_ref().and_then(|d| d.commit.as_ref()) == t.approved_commit.as_ref()) =>
            {
                go(Ready)
            }
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
        (Event::Accept, Running | WaitingYou) if t.write => Err(format!(
            "{}: it is still {}: wait for its report or stop it first",
            t.label,
            t.state_text()
        )),
        (Event::Accept, Ready | Merging | Archived | Queued) if t.write => Err(format!(
            "{}: can't ship it without review: it is {} already",
            t.label,
            t.state_text()
        )),
        // Shipped without an approved review: the user's call (the caller checks its commits
        // and asks on a card first).
        (Event::Accept, _) if t.write => stop_review(Ready),
        (Event::Review, Reviewing) if !t.write && !reviewing => {
            with(Reviewing, Effect::StartReview { uses_round: true })
        }
        // The stable code-written prefix also recognizes review failures on existing boards.
        // Retry the same evidence and round, but resolve a fresh reviewer in the caller.
        (Event::Review, Blocked)
            if t.done.is_some() && block_is(|b| !b.merge && b.reason.starts_with("its review can't start:")) =>
        {
            with(
                Reviewing,
                Effect::StartReview {
                    uses_round: !t.review_free,
                },
            )
        }
        // A write task that keeps ending without a report, or reported `blocked`: the caller records
        // its commits first (R23, L10.9); a head already approved needs no new review (R27).
        (Event::Review, Blocked) if t.write && block_is(|b| !b.merge) && t.approval_covers_done() => go(Ready),
        (Event::Review, Blocked) if t.write && block_is(|b| !b.merge) => {
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

        (Event::AfterSteps, Ready)
            if t.landed == Some(Landed::Merged) && !t.after.is_empty() && t.after_state == AfterState::NotRun =>
        {
            go(Running)
        }
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
        Event::AfterSteps => "after-merge steps",
        Event::Retire => "retire",
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Board {
    /// Last 4 characters of the running super session's id: it numbers its tasks from T1.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub sid: String,
    pub tasks: Vec<Task>,
    /// Repo main checkout → task id holding its merge lock.
    pub merge_locks: BTreeMap<PathBuf, String>,
    /// Provider → the limit holding it.
    pub holds: BTreeMap<String, Limit>,
    /// Unix seconds of super's last health check that found everything fine.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_check: Option<u64>,
    /// Open questions and warnings for the user (Needs you), oldest first.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub needs: Vec<Need>,
    /// The General charter (L10.9): the user's answers to General questions and super's
    /// `task note`s on General, written only by code, the newest [`GENERAL_KEPT`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub general: Vec<String>,
}

/// General charter points kept.
pub const GENERAL_KEPT: usize = 30;

fn is_default<T: Default + PartialEq>(t: &T) -> bool {
    *t == T::default()
}

/// A list of points, from a list, one string (one point; an old board's scope) or null.
pub fn points<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Vec<String>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum P {
        One(String),
        Many(Vec<String>),
    }
    Ok(match Option::<P>::deserialize(d)? {
        None => vec![],
        Some(P::One(s)) => vec![s].into_iter().filter(|s| !s.trim().is_empty()).collect(),
        Some(P::Many(v)) => v,
    })
}

impl Board {
    pub fn task(&self, id: &str) -> Option<&Task> {
        self.tasks.iter().find(|t| t.id == id)
    }

    /// The running super session's next task number: its rows (reviews too) count from T1.
    pub fn next_number(&self) -> u64 {
        self.tasks
            .iter()
            .filter(|t| t.sid == self.sid)
            .map(Task::number)
            .max()
            .unwrap_or(0)
            + 1
    }

    /// A new row's id and label: `<sid4>-T<n>-<slug>` (worktree and branch names follow it).
    pub fn with_sid(&self, short: &str) -> String {
        match self.sid.as_str() {
            "" => short.to_string(),
            s => format!("{s}-{short}"),
        }
    }

    /// How super shows `t` now ([`Task::name`]).
    pub fn name(&self, t: &Task) -> String {
        t.name(&self.sid)
    }

    /// The task labelled `label`, or shown as `label` ([`name`](Self::name)).
    /// Refuses when one of `labels` names a task the user paused (L10.9): super may not
    /// ask about it or message it; only the user resumes it.
    pub fn user_paused<'a>(&self, labels: impl IntoIterator<Item = &'a str>) -> Result<(), String> {
        match labels
            .into_iter()
            .filter_map(|l| self.by_label(l))
            .find(|t| t.hold.as_ref().is_some_and(|h| h.user))
        {
            Some(t) => Err(paused(&self.name(t))),
            None => Ok(()),
        }
    }

    pub fn by_label(&self, label: &str) -> Option<&Task> {
        self.tasks.iter().find(|t| t.label == label || self.name(t) == label)
    }

    /// [`Task::status_line`] of the task labelled `label`, now, under its shown name.
    pub fn status_line(&self, label: &str) -> Option<String> {
        self.by_label(label).map(|t| {
            let l = t.status_line(now());
            match l.strip_prefix(&t.label) {
                Some(rest) => format!("{}{rest}", self.name(t)),
                None => l,
            }
        })
    }

    /// A `say` was sent: a `question` or `warning` joins Needs you; an `update` or `done`
    /// resolves the warnings about any of its tasks; a `reply` answers typed questions.
    pub fn note_say(&mut self, id: &str, kind: &str, tasks: &[String], status: &str, typed: bool) {
        match kind {
            "update" | "done" if !tasks.is_empty() => self
                .needs
                .retain(|n| n.kind != "warning" || !n.tasks.iter().any(|t| tasks.contains(t))),
            "reply" => self.needs.retain(|n| !n.typed),
            _ => {}
        }
        if kind == "question" {
            let ids: Vec<String> = tasks
                .iter()
                .filter_map(|l| self.by_label(l))
                .map(|t| t.id.clone())
                .collect();
            for t in self.tasks.iter_mut().filter(|t| ids.contains(&t.id)) {
                t.acted_at = now();
            }
        }
        if matches!(kind, "question" | "warning") {
            self.needs.push(Need {
                id: id.into(),
                kind: kind.into(),
                tasks: tasks.to_vec(),
                status: status.into(),
                at: now(),
                typed,
                card: None,
            });
        }
    }

    /// The stalled work tasks at `now` and for how long (L10.9): blocked, stopped, or ready
    /// with no landed merge; no open question to the user about it (a `say` question, its
    /// own relayed card, code's deploy card); no change and no action of super's on it for
    /// `after` seconds (0 = off).
    pub fn stalled(&self, now: u64, after: u64) -> Vec<(&Task, u64)> {
        if after == 0 {
            return vec![];
        }
        let asked = |t: &Task| {
            self.needs
                .iter()
                .any(|n| n.kind == "question" && n.tasks.iter().any(|l| self.by_label(l).is_some_and(|r| r.id == t.id)))
        };
        self.tasks
            .iter()
            .filter(|t| t.kind == Kind::Work && t.hold.is_none())
            .filter(|t| match t.state {
                State::Blocked | State::Stopped => true,
                State::Ready => t.landed != Some(Landed::Merged),
                _ => false,
            })
            .filter(|t| !t.asking && t.health.cards == 0 && !asked(t))
            .map(|t| (t, now.saturating_sub(t.updated.max(t.acted_at))))
            .filter(|(_, s)| *s >= after)
            .collect()
    }

    /// Needs you for the windows' Tasks tab.
    pub fn needs_rows(&self) -> Vec<novacode_tui::panel::NeedRow> {
        self.needs
            .iter()
            .map(|n| novacode_tui::panel::NeedRow {
                id: n.id.clone(),
                kind: n.kind.clone(),
                tasks: n.tasks.clone(),
                status: n.status.clone(),
                at_ms: n.at * 1000,
            })
            .collect()
    }

    /// Take `id` off Needs you (its card was answered, or the user dismissed it).
    /// Keep `text` on the General charter, dropping the oldest past [`GENERAL_KEPT`].
    pub fn note_general(&mut self, text: String) {
        let text = text.trim();
        if text.is_empty() {
            return;
        }
        self.general.push(text.to_string());
        let extra = self.general.len().saturating_sub(GENERAL_KEPT);
        self.general.drain(..extra);
    }

    pub fn drop_need(&mut self, id: &str) -> bool {
        let n = self.needs.len();
        self.needs.retain(|x| x.id != id);
        self.needs.len() < n
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
        // Super's own hold lasts until the task reports; the user's until they resume it.
        if matches!(ev, Event::Report { .. }) && t.hold.as_ref().is_some_and(|h| !h.user) {
            t.hold = None;
        }
        // A report replaces a provisional block even when the state stays `blocked`.
        if n.from != n.to || matches!(ev, Event::Report { .. }) && n.block.is_some() {
            t.block = n.block.clone();
            if n.to != State::Blocked {
                t.question = None;
            }
        }
        // While its after-merge steps run, the work stays merged.
        if ev == Event::AfterSteps {
            t.after_state = AfterState::Running;
            // The merged commit is the one the user approved; a later commit gets a review.
            t.approved_commit = t.done.as_ref().and_then(|d| d.commit.clone());
        } else if n.to == State::Running && t.after_state != AfterState::Running {
            t.landed = None;
            t.accepted = false;
            t.after_state = AfterState::NotRun;
        }
        if t.after_state == AfterState::Running && n.to == State::Ready {
            t.after_state = AfterState::Done;
        }
        if ev == Event::Accept && t.write {
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
            Event::Report {
                state,
                summary,
                question,
            } => {
                t.summary = Some(summary.clone());
                t.question = question.clone();
                t.report_at = now();
                if *state == ReportState::Progress {
                    t.health.progress_at = now();
                }
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
                    label: self.name(t),
                    repo: t.repo_name(),
                    state: t.state_text(),
                    model,
                    tokens,
                    started_ms: t.started * 1000,
                    summary: t.summary.clone(),
                    session: t.session.clone(),
                    review: t.kind == Kind::Review,
                }
            })
            .collect()
    }

    /// Super's state block (L10.9), rendered fresh for every request: the rows not
    /// archived (label, state, last report summary), each such work task's charter, and
    /// the user's open requests (super's open `todo` items).
    pub fn state_block(&self, open: &[String]) -> String {
        let cell = |s: &str| s.replace('|', "\\|").replace('\n', " ");
        let rows: Vec<&Task> = self.tasks.iter().filter(|t| t.state != State::Archived).collect();
        let mut s =
            String::from("Written by code for this request; the board, not the chat, is the truth.\n\n## Board\n");
        if rows.is_empty() {
            s.push_str("(no tasks)\n");
        } else {
            s.push_str("| Task | State | Last report |\n|---|---|---|\n");
        }
        for t in &rows {
            let summary: String = t.summary.as_deref().unwrap_or("").chars().take(STATE_SUMMARY).collect();
            let state = match t.hold_text() {
                Some(h) => format!("{} · {h}", t.state_text()),
                None => t.state_text(),
            };
            s.push_str(&format!(
                "| {} | {} | {} |\n",
                cell(&self.name(t)),
                cell(&state),
                cell(&summary)
            ));
        }
        let archived = self.tasks.len() - rows.len();
        if archived > 0 {
            s.push_str(&format!(
                "({archived} archived; `task list {{all: true}}` shows them)\n"
            ));
        }
        let running: Vec<&&Task> = rows
            .iter()
            .filter(|t| matches!(t.state, State::Running | State::WaitingYou) && t.hold.is_none())
            .collect();
        if !running.is_empty() {
            let now = now();
            s.push_str("\n## Health (running tasks)\n");
            if let Some(c) = self.last_check {
                s.push_str(&format!("Your last check that found all fine: {} ago.\n", age(now, c)));
            }
            for t in running {
                s.push_str(&format!("- {}: {}\n", self.name(t), t.health.line(now)));
            }
        }
        let charters: Vec<String> = rows
            .iter()
            .filter(|t| t.kind == Kind::Work)
            .map(|t| {
                let c = t.charter();
                c.replacen(&format!("### {} ", t.label), &format!("### {} ", self.name(t)), 1)
            })
            .collect();
        if !charters.is_empty() {
            s.push_str("\n## Charters\n");
            s.push_str(&charters.join("\n"));
        }
        // The General charter: answers to General questions, kept so super never asks again.
        if !self.general.is_empty() {
            s.push_str("\n## General decisions\n");
            for g in &self.general {
                s.push_str(&format!("- {}\n", cell(g)));
            }
        }
        s.push_str("\n## Open requests\n");
        if open.is_empty() {
            s.push_str("(none)\n");
        }
        for o in open {
            s.push_str(&format!("- {o}\n"));
        }
        // Open on the user's list; a question's card is open in their windows.
        if !self.needs.is_empty() {
            s.push_str("\n## Needs you\n");
        }
        for n in &self.needs {
            let tasks = match n.tasks.is_empty() {
                true => String::new(),
                false => format!(" ({})", n.tasks.join(", ")),
            };
            s.push_str(&format!("- {}{tasks}: {}\n", n.kind, cell(&n.status)));
        }
        s
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
                cell(&self.name(t)),
                cell(&t.repo_name()),
                cell(&t.state_text()),
                cell(&t.model),
                cell(t.summary.as_deref().unwrap_or("")),
            ));
        }
        let after = self
            .tasks
            .iter()
            .any(|t| t.state != State::Archived && !t.after.is_empty());
        if !self.merge_locks.is_empty() || !self.holds.is_empty() || self.last_check.is_some() || after {
            out.push('\n');
        }
        if let Some(c) = self.last_check {
            out.push_str(&format!(
                "- Last health check: {}Z (all fine)\n",
                novacode_core::memory::utc_minute(c * 1000)
            ));
        }
        for (repo, id) in &self.merge_locks {
            out.push_str(&format!("- Merge lock: {} held by {id}\n", repo.display()));
        }
        for t in self
            .tasks
            .iter()
            .filter(|t| t.state != State::Archived && !t.after.is_empty())
        {
            out.push_str(&format!("- After merge, {}: {}\n", self.name(t), t.after.join("; ")));
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

    #[test]
    fn stalled_tasks_are_unfinished_with_no_question_and_no_action_for_long() {
        let now = 10_000;
        let at = |id: &str, state: State, updated: u64| Task {
            id: id.into(),
            label: id.into(),
            kind: Kind::Work,
            state,
            updated,
            ..Task::default()
        };
        let mut b = Board {
            tasks: vec![
                at("T1", State::Blocked, 1000),
                at("T2", State::Stopped, 1000),
                at("T3", State::Ready, 1000),
                at("T4", State::Running, 1000),
                at("T5", State::Blocked, now - 60),
                at("T6", State::Ready, 1000),
                at("T7", State::Blocked, 1000),
                at("T8", State::Blocked, 1000),
                at("T9", State::Ready, 1000),
                at("T10", State::Blocked, 1000),
            ],
            ..Board::default()
        };
        b.tasks[5].landed = Some(Landed::Merged);
        b.tasks[8].landed = Some(Landed::PrOpened);
        b.tasks[6].acted_at = now - 60;
        b.tasks[7].health.cards = 1;
        b.tasks[9].asking = true;
        b.note_say("q1", "question", &["T2".into()], "T2 needs a port", false);
        let ids = |b: &Board, after| -> Vec<(String, u64)> {
            b.stalled(now, after)
                .into_iter()
                .map(|(t, s)| (t.id.clone(), s))
                .collect()
        };
        // T2 waits on the user's answer; T4 runs; T5 changed and T7 was acted on a minute
        // ago; T6 landed; T8's own card and T10's deploy card are open.
        assert_eq!(
            ids(&b, 900),
            [
                ("T1".to_string(), 9000),
                ("T3".to_string(), 9000),
                ("T9".to_string(), 9000)
            ]
        );
        assert!(ids(&b, 0).is_empty(), "0 = off");
        // A `say` question naming a task is super's action on it.
        b.drop_need("q1");
        b.note_say("q2", "question", &["T1".into()], "T1?", false);
        assert!(b.tasks[0].acted_at > 0);
        assert!(b.tasks[1].acted_at > 0, "T2 was asked about too");
        assert_eq!(ids(&b, 900), [("T3".to_string(), 9000), ("T9".to_string(), 9000)]);
    }

    #[test]
    fn a_status_line_is_built_from_the_board_row() {
        let now = now();
        let mut t = row(State::Running);
        assert_eq!(t.status_line(now), "T1-fix · running · no report yet");
        t.summary = Some("parser done, 40% of the tests pass\nnext: lexer".into());
        t.report_at = now - 720;
        assert_eq!(
            t.status_line(now),
            "T1-fix · running · 40% · parser done, 40% of the tests pass next: lexer · last report 12 min ago"
        );
        t.summary = Some("x".repeat(200));
        let l = t.status_line(now);
        assert!(l.contains(&format!("{}…", "x".repeat(STATUS_SUMMARY))), "{l}");
        let b = Board {
            tasks: vec![t],
            ..Default::default()
        };
        assert!(b.status_line("T1-fix").unwrap().starts_with("T1-fix · running"));
        assert_eq!(b.status_line("T9"), None);
    }

    #[test]
    fn a_report_sets_the_report_time() {
        let mut b = Board {
            tasks: vec![row(State::Running)],
            ..Default::default()
        };
        b.apply("T1", report(ReportState::Progress)).unwrap();
        assert!(b.tasks[0].report_at >= now() - 1);
    }

    #[test]
    fn needs_you_holds_questions_and_warnings_until_answered_resolved_or_dismissed() {
        let mut b = Board::default();
        let t = |l: &[&str]| l.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        b.note_say("c1", "warning", &t(&["T1"]), "T1 stuck", false);
        b.note_say("c2", "question", &t(&["T2"]), "T2 needs a port", false);
        b.note_say("c3", "question", &t(&[]), "which name?", true);
        b.note_say("c4", "note", &t(&["T1"]), "", false);
        assert_eq!(
            b.needs.iter().map(|n| n.id.as_str()).collect::<Vec<_>>(),
            ["c1", "c2", "c3"]
        );
        // An update on another task resolves nothing; one on T1 resolves its warning.
        b.note_say("c5", "update", &t(&["T2"]), "", false);
        assert_eq!(b.needs.len(), 3);
        b.note_say("c6", "done", &t(&["T1"]), "", false);
        assert_eq!(b.needs.iter().map(|n| n.id.as_str()).collect::<Vec<_>>(), ["c2", "c3"]);
        // A card answered; a typed question answered by super's next reply.
        assert!(b.drop_need("c2"));
        b.note_say("c7", "reply", &t(&[]), "", false);
        assert!(b.needs.is_empty());
        // Dismissed by the user, and kept across a save.
        b.note_say("c8", "warning", &t(&[]), "disk full", false);
        let d = tempfile::tempdir().unwrap();
        b.save(d.path()).unwrap();
        let mut b = Board::load(d.path()).unwrap();
        assert_eq!(b.needs[0].status, "disk full");
        assert!(b.drop_need("c8") && !b.drop_need("c8"));
    }

    /// S4: super sees the open Needs you rows (after a restart its chat may not show them).
    #[test]
    fn the_state_block_lists_needs_you() {
        let mut b = Board::default();
        assert!(!b.state_block(&[]).contains("## Needs you"));
        b.note_say("c1", "question", &["T1".into()], "T1 needs a port", false);
        let s = b.state_block(&[]);
        assert!(s.contains("## Needs you\n- question (T1): T1 needs a port\n"), "{s}");
    }

    #[test]
    fn the_state_block_has_rows_charters_and_open_requests() {
        let mut t = row(State::Running);
        t.request = "fix the parser".into();
        t.criteria = vec!["tabs parse".into()];
        t.summary = Some("halfway".into());
        t.notes = vec!["stop before deploy".into()];
        let mut old = row(State::Archived);
        old.label = "T0-old".into();
        old.request = "OLD BRIEF".into();
        let b = Board {
            tasks: vec![t, old],
            ..Default::default()
        };
        let s = b.state_block(&["look at the logs tomorrow".into()]);
        assert!(s.contains("| T1-fix | running | halfway |"), "{s}");
        assert!(
            s.contains("### T1-fix charter\nBrief: fix the parser\nCriteria:\n1. tabs parse\n"),
            "{s}"
        );
        assert!(
            s.contains("- stop before deploy") && s.contains("- look at the logs tomorrow"),
            "{s}"
        );
        assert!(!s.contains("OLD BRIEF") && s.contains("1 archived"), "{s}");
        // An old board without notes still loads.
        let t: Task = serde_json::from_str(r#"{"id":"T1","label":"T1-x"}"#).unwrap();
        assert!(t.notes.is_empty());
    }

    /// L10.9: General answers and notes stay on the board (newest kept) and in the state block.
    #[test]
    fn general_decisions_are_kept_newest_and_shown_in_the_state_block() {
        let mut b = Board::default();
        assert!(!b.state_block(&[]).contains("General decisions"));
        for i in 0..GENERAL_KEPT + 2 {
            b.note_general(format!("n{i}"));
        }
        b.note_general("  ".into());
        assert_eq!(b.general.len(), GENERAL_KEPT);
        assert_eq!(b.general[0], "n2");
        let s = b.state_block(&[]);
        assert!(s.contains("## General decisions\n- n2\n"), "{s}");
        let saved: Board = serde_json::from_str(&serde_json::to_string(&b).unwrap()).unwrap();
        assert_eq!(saved.general, b.general);
        // An old board without them still loads.
        assert!(
            serde_json::from_str::<Board>(r#"{"tasks":[],"merge_locks":{},"holds":{}}"#)
                .unwrap()
                .general
                .is_empty()
        );
    }

    /// L10.9 holds: a held task is not stalled and has no Health line; the state block
    /// names the hold; a report or super's work clears super's hold, never the user's.
    #[test]
    fn held_tasks_are_not_stalled_and_the_state_block_names_the_hold() {
        let now = 10_000;
        let at = |id: &str, state: State| Task {
            id: id.into(),
            label: format!("{id}-x"),
            kind: Kind::Work,
            state,
            updated: 1000,
            ..Task::default()
        };
        let mut b = Board {
            tasks: vec![
                at("T1", State::Blocked),
                at("T2", State::Stopped),
                at("T3", State::Blocked),
                at("T4", State::Running),
            ],
            ..Board::default()
        };
        b.tasks[0].hold = Some(Hold {
            reason: "waits for T3's merge".into(),
            user: false,
            at: 1,
        });
        b.tasks[1].hold = Some(Hold {
            reason: "user is away".into(),
            user: true,
            at: 1,
        });
        b.tasks[3].hold = Some(Hold {
            reason: "later".into(),
            user: false,
            at: 1,
        });
        let ids: Vec<&str> = b.stalled(now, 900).into_iter().map(|(t, _)| t.id.as_str()).collect();
        assert_eq!(ids, ["T3"]);
        let s = b.state_block(&[]);
        assert!(s.contains("| T1-x | blocked · held: waits for T3's merge |"), "{s}");
        assert!(s.contains("| T2-x | stopped · paused by you: user is away |"), "{s}");
        assert!(!s.contains("## Health"), "a held task gets no health line: {s}");
        // Only the user resumes a task they paused.
        assert!(b.user_paused(["T1-x", "General"]).is_ok());
        assert_eq!(
            b.user_paused(["T3-x", "T2-x"]).unwrap_err(),
            "the user paused T2-x; only the user resumes it"
        );
        // A report clears super's hold, not the user's.
        b.tasks[0].state = State::Running;
        b.tasks[1].state = State::Running;
        b.apply("T1", report(ReportState::Blocked)).unwrap();
        assert_eq!(b.task("T1").unwrap().hold, None);
        b.apply("T2", report(ReportState::Blocked)).unwrap();
        assert!(b.task("T2").unwrap().hold.is_some());
        // An old row has no hold, and none is saved.
        let t: Task = serde_json::from_str(r#"{"id":"T1","label":"T1-x"}"#).unwrap();
        assert_eq!(t.hold, None);
        assert!(!serde_json::to_string(&t).unwrap().contains("hold"));
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
            since: None,
        }
    }

    #[test]
    fn a_rate_limit_hold_lifts_at_most_an_hour_after_it_started_and_an_old_one_at_once() {
        let rl = |reset_at, since| Limit {
            kind: LimitKind::RateLimit,
            reset_at,
            since,
            ..limit()
        };
        assert_eq!(rl(Some(5000), Some(1000)).lifts_at(), Some(1000 + RATE_LIMIT_HOLD_MAX));
        assert_eq!(rl(Some(1500), Some(1000)).lifts_at(), Some(1500));
        assert_eq!(rl(None, Some(1000)).lifts_at(), Some(1000 + RATE_LIMIT_HOLD_MAX));
        // An old board's hold (no start): due now.
        let old: Limit =
            serde_json::from_str(r#"{"kind":"rate-limit","provider":"claude","reset_at":1794239999}"#).unwrap();
        assert_eq!(old.since, None);
        assert!(old.lifts_at().unwrap() <= now());
        // A quota hold keeps its reset time.
        let q = Limit {
            reset_at: Some(1794239999),
            since: Some(1000),
            ..limit()
        };
        assert_eq!(q.lifts_at(), Some(1794239999));
    }

    fn to(t: &Task, ev: Event) -> State {
        next(t, &ev, None)
            .unwrap_or_else(|e| panic!("{ev:?} from {:?} refused: {e}", t.state))
            .to
    }

    fn effects(t: &Task, ev: Event) -> Vec<Effect> {
        next(t, &ev, None).unwrap().effects
    }

    /// A new commit made while the after steps run is unreviewed work: its `done` is reviewed.
    #[test]
    fn a_new_commit_during_after_steps_is_reviewed() {
        let mut t = row(Merging);
        t.after = vec!["deploy to prod".into()];
        t.done = Some(Done {
            commit: Some("c1".into()),
            ..Done::default()
        });
        let mut b = Board {
            tasks: vec![t],
            ..Default::default()
        };
        b.apply("T1", Event::MergeReport(MergeResult::Merged)).unwrap();
        b.apply("T1", Event::AfterSteps).unwrap();
        let mut same = b.clone();
        let n = same.apply("T1", report(ReportState::Done)).unwrap();
        assert_eq!((n.to, n.effects), (Ready, vec![]), "the merged commit itself");
        b.tasks[0].done.as_mut().unwrap().commit = Some("c2".into());
        let n = b.apply("T1", report(ReportState::Done)).unwrap();
        assert_eq!(n.to, Reviewing, "a new commit is reviewed");
    }

    #[test]
    fn accept_refuses_a_live_write_task() {
        for s in [Running, WaitingYou] {
            assert!(next(&row(s), &Event::Accept, None).is_err(), "{s:?}");
        }
    }

    /// L10.9 item 2: after-merge steps (deploy) run once the merge landed; their `done`
    /// needs no review and marks the row; a blocked step blocks as usual.
    #[test]
    fn after_steps_run_once_merged_and_their_done_needs_no_review() {
        let mut t = row(Merging);
        t.after = vec!["deploy to prod".into()];
        let mut b = Board {
            tasks: vec![t],
            ..Default::default()
        };
        assert!(b.apply("T1", Event::AfterSteps).is_err(), "not before the merge landed");
        b.apply("T1", Event::MergeReport(MergeResult::Merged)).unwrap();
        assert_eq!(b.task("T1").unwrap().state_text(), "ready (merged)");
        assert_eq!(b.apply("T1", Event::AfterSteps).unwrap().to, Running);
        let t = b.task("T1").unwrap();
        assert_eq!(t.landed, Some(Landed::Merged), "still merged while the steps run");
        assert_eq!(t.state_text(), "running (merged, after steps)");
        // A blocked step blocks; a send goes on with the steps, still merged.
        b.apply("T1", report(ReportState::Blocked)).unwrap();
        b.apply("T1", Event::Send).unwrap();
        assert_eq!(b.task("T1").unwrap().landed, Some(Landed::Merged));
        let n = b.apply("T1", report(ReportState::Done)).unwrap();
        assert_eq!((n.to, n.effects), (Ready, vec![]), "no review of the after steps");
        let t = b.task("T1").unwrap();
        assert_eq!(t.state_text(), "ready (merged, after steps done)");
        assert!(b.clone().apply("T1", Event::AfterSteps).is_err(), "they ran once");
        // New work after them: unmerged again, and its done is reviewed.
        b.apply("T1", Event::Send).unwrap();
        let t = b.task("T1").unwrap();
        assert_eq!((t.landed, t.after_state), (None, AfterState::NotRun));
        assert!(matches!(
            effects(t, report(ReportState::Done))[..],
            [Effect::StartReview { .. }]
        ));
    }

    /// An old board row has no `after`: the merge ends as before and nothing runs after it.
    #[test]
    fn an_old_row_without_after_steps_merges_as_before() {
        let t: Task = serde_json::from_str(
            r#"{"id":"T1","label":"T1-x","state":"merging","write":true,"repo":"/r","branch":"b"}"#,
        )
        .unwrap();
        assert!(t.after.is_empty() && t.after_state == AfterState::NotRun);
        let mut b = Board {
            tasks: vec![t],
            ..Default::default()
        };
        b.apply("T1", Event::MergeReport(MergeResult::Merged)).unwrap();
        assert!(b.apply("T1", Event::AfterSteps).is_err());
        assert_eq!(b.task("T1").unwrap().state_text(), "ready (merged)");
        assert!(!b.render().contains("After merge") && !b.task("T1").unwrap().charter().contains("After merge"));
        let json = serde_json::to_string(&b.tasks[0]).unwrap();
        assert!(!json.contains("after"), "{json}");
    }

    #[test]
    fn after_steps_show_in_the_charter_and_board_md_and_a_charter_limit_holds_them() {
        let mut t = row(Ready);
        t.after = vec!["deploy".into(), "check the live site".into()];
        let c = t.charter();
        assert!(
            c.contains("After merge (run only once the merge lands; not judged by the review):\n1. deploy\n2. check the live site\n"),
            "{c}"
        );
        let b = Board {
            tasks: vec![t.clone()],
            ..Default::default()
        };
        assert!(
            b.render()
                .contains("- After merge, T1-fix: deploy; check the live site"),
            "{}",
            b.render()
        );
        assert_eq!(t.after_limit(), None);
        t.notes = vec!["user: use port 80".into(), "Stop before deploy, I'll do it".into()];
        assert_eq!(t.after_limit(), Some("Stop before deploy, I'll do it"));
        t.notes = vec!["don't deploy on Fridays".into()];
        assert!(t.after_limit().is_some());
        t.notes.clear();
        t.scope = vec!["src/ only".into(), "do not deploy".into()];
        assert!(t.after_limit().is_some());
        // The brief and criteria count too (novahome T2: "no push, no deploy").
        t.scope.clear();
        t.request = "Fix the parser; no push, no deploy.".into();
        assert_eq!(t.after_limit(), Some("Fix the parser; no push, no deploy."));
        t.request = "Fix the parser".into();
        t.criteria = vec!["tests pass".into(), "stop before push".into()];
        assert_eq!(t.after_limit(), Some("stop before push"));
        t.criteria = vec!["no push to main".into()];
        assert!(t.after_limit().is_some());
    }

    /// Scope is a list of short points; an old board's string scope loads as one point,
    /// and the charter shows the points as bullets.
    #[test]
    fn scope_is_points_and_an_old_string_scope_loads_as_one_point() {
        let t: Task = serde_json::from_str(r#"{"id":"T1","label":"T1-x","scope":"src/ only"}"#).unwrap();
        assert_eq!(t.scope, ["src/ only"]);
        let t: Task = serde_json::from_str(r#"{"id":"T1","label":"T1-x","scope":null}"#).unwrap();
        assert!(t.scope.is_empty());
        let mut t: Task = serde_json::from_str(r#"{"id":"T1","label":"T1-x","scope":["src/","tests/"]}"#).unwrap();
        assert_eq!(t.scope, ["src/", "tests/"]);
        assert!(t.charter().contains("Scope:\n- src/\n- tests/\n"), "{}", t.charter());
        let back: Task = serde_json::from_str(&serde_json::to_string(&t).unwrap()).unwrap();
        assert_eq!(back.scope, t.scope);
        t.scope.clear();
        assert!(!serde_json::to_string(&t).unwrap().contains("scope"));
        assert!(!t.charter().contains("Scope"));
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
        // A write task under review ships without it only on the user's card (the caller asks).
        let n = next(&row(Reviewing), &Event::Accept, None).unwrap();
        assert_eq!((n.to, n.effects), (Ready, vec![]));
    }

    #[test]
    fn a_write_task_out_of_review_rounds_can_be_accepted_and_is_marked() {
        let used_up = Task {
            block: Some(Block::new(ROUNDS_USED_UP)),
            ..row(Blocked)
        };
        assert_eq!(to(&used_up, Event::Accept), Ready);
        assert_eq!(
            to(&row(Blocked), Event::Accept),
            Ready,
            "other blocks: the caller's card"
        );
        for s in [Ready, Merging, Archived, Queued] {
            assert!(next(&row(s), &Event::Accept, None).is_err(), "{s:?}");
        }
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
        // A block the read task reported still refuses accept.
        let mut b = Board {
            tasks: vec![read(Running)],
            ..Board::default()
        };
        b.apply("T1", report(ReportState::Blocked)).unwrap();
        assert!(b.apply("T1", Event::Accept).is_err(), "reported block");
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
    fn explicit_review_retry_keeps_evidence_and_round_policy_on_existing_boards() {
        for free in [false, true] {
            let task = Task {
                state: Blocked,
                block: Some(Block::new("its review can't start: provider is not logged in")),
                done: Some(Done {
                    commit: Some("pinned".into()),
                    ..Done::default()
                }),
                review_free: free,
                review_rounds: 1,
                ..row(Reviewing)
            };
            // Persisted rows predate this policy; no new marker is required to retry them.
            let task: Task = serde_json::from_value(serde_json::to_value(task).unwrap()).unwrap();
            let done = task.done.clone();
            let mut board = Board {
                tasks: vec![task],
                ..Board::default()
            };
            let n = board.apply("T1", Event::Review).unwrap();
            assert_eq!(n.effects, vec![Effect::StartReview { uses_round: !free }]);
            let task = board.task("T1").unwrap();
            assert_eq!(task.state, Reviewing);
            assert_eq!(task.done, done);
            assert_eq!((task.review_free, task.review_rounds), (free, 1));
        }
    }

    #[test]
    fn review_from_a_code_made_or_reported_block_uses_a_round() {
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
        // L10.9: a block the task reported too (the caller checks its commits first).
        let reported = Task {
            block: Some(Block::new("reported blocked")),
            ..row(Blocked)
        };
        let n = next(&reported, &Event::Review, None).unwrap();
        assert_eq!(
            (n.to, n.effects),
            (Reviewing, vec![Effect::StartReview { uses_round: true }])
        );
        let merge = Task {
            block: Some(Block::new("conflict").merge()),
            ..row(Blocked)
        };
        let e = next(&merge, &Event::Review, None).unwrap_err();
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
            (Merging, Event::Accept),
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
            sid: String::new(),
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
            last_check: None,
            needs: vec![],
            general: vec![],
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
    fn the_state_block_shows_health_facts_for_running_tasks_and_board_md_the_last_check() {
        let now = now();
        let mut t = row(State::Running);
        t.summary = Some("3 of 5 files done".into());
        t.health = Health {
            last_event: now - 120,
            tool: Some("bash".into()),
            tool_since: now - 900,
            errors: 3,
            repeats: 4,
            cards: 1,
            card_since: now - 600,
            progress_at: now - 1500,
        };
        let mut idle = row(State::Ready);
        idle.label = "T2-done".into();
        idle.health.errors = 9;
        let mut b = Board {
            tasks: vec![t, idle],
            ..Default::default()
        };
        let s = b.state_block(&[]);
        assert!(
            s.contains(
                "- T1-fix: last event 2 min ago; running `bash` for 15 min; 3 tool errors in a row; the same \
                 call 4 times in a row; 1 unanswered card, oldest 10 min; last progress 25 min ago"
            ),
            "{s}"
        );
        assert!(!s.contains("T2-done: last event"), "only running tasks: {s}");
        // No tool, no progress yet.
        b.tasks[0].health = Health {
            last_event: now,
            ..Health::default()
        };
        let s = b.state_block(&[]);
        assert!(
            s.contains("- T1-fix: last event 0 s ago; no tool running; no progress report yet"),
            "{s}"
        );
        assert!(!b.render().contains("Last health check"));
        b.last_check = Some(1_800_000_000);
        assert!(
            b.render().contains("- Last health check: 2027-01-15 08:00Z (all fine)"),
            "{}",
            b.render()
        );
        // An old board without health facts still loads.
        let t: Task = serde_json::from_str(r#"{"id":"T1","label":"T1-x"}"#).unwrap();
        assert_eq!(t.health, Health::default());
    }

    #[test]
    fn a_progress_report_records_its_time() {
        let mut b = board(&[("T1", Running)]);
        b.apply("T1", report(ReportState::Progress)).unwrap();
        assert!(b.tasks[0].health.progress_at >= now() - 1);
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
