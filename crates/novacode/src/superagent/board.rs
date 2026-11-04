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

/// The state a new task starts in (the two spawn rows).
pub fn spawn_state(can_start: bool) -> State {
    if can_start { State::Running } else { State::Queued }
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LimitKind {
    Quota,
    RateLimit,
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
}

impl Block {
    fn new(reason: impl Into<String>) -> Block {
        Block {
            reason: reason.into(),
            limit: None,
        }
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
    /// One line per check: its recorded command, folder and exit code.
    pub checks: Vec<String>,
    /// Screenshot paths and URLs.
    pub artifacts: Vec<String>,
}

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
    pub landed: Option<Landed>,
    /// The user's words, sent in the brief (kept so a queued task can start later).
    pub request: String,
    /// Skills the brief names.
    pub skills: Vec<String>,
    /// `task spawn {goal: true}`: an open-ended task.
    pub goal: bool,
    /// Messages sent while `queued`: added to the brief.
    pub pending: Vec<String>,
    /// The task's session id, once its process started.
    pub session: Option<String>,
    /// The folder its process runs in (absolute; in its worktree for a write task).
    pub cwd: Option<PathBuf>,
    /// Hash of super's last prompt to it ([`prompt_hash`]): the catch-up after a
    /// reconnect finds it in the task's messages.
    pub last_prompt: Option<String>,
    /// Unix seconds.
    pub started: u64,
    pub updated: u64,
}

impl Task {
    /// The state with its block reason or landing mark: `blocked: quota`, `ready (merged)`.
    pub fn state_text(&self) -> String {
        let state = self.state.name();
        match (&self.block, self.landed) {
            (Some(b), _) => format!("{state}: {}", b.reason),
            (None, Some(Landed::Merged)) => format!("{state} (merged)"),
            (None, Some(Landed::PrOpened)) => format!("{state} (PR opened)"),
            _ => state.to_string(),
        }
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
            Event::TurnUnreported | Event::TurnError(_) | Event::TurnLimit(_) => Some(Blocked),
            Event::ProcessExit => Some(Failed),
            Event::Restart => Some(Interrupted),
            Event::Close => Some(Archived),
            _ => None,
        };
        if let Some(to) = to {
            let mut n = with(to, Effect::CheckMergeLeft)?;
            if to == Blocked {
                n.block = Some(turn_block(ev));
            }
            return Ok(n);
        }
    }

    let reviewing = t.reviewer.is_some();
    match (ev, from) {
        (Event::SlotFree, Queued) => go(Running),

        (Event::CardOpened, Running | WaitingYou) => go(WaitingYou),
        (Event::CardOpened, Merging | Reviewing) => with(from, Effect::RelayCard),
        (Event::LastCardAnswered, WaitingYou) => go(Running),

        (Event::Report { state, .. }, _) => match (state, from) {
            (ReportState::Progress, s) if s.is_live() => go(s),
            (ReportState::Blocked, Running) => blocked(Block::new("reported blocked")),
            (ReportState::Failed, Running) => go(Failed),
            (ReportState::Done, Running) if t.write => with(Reviewing, Effect::StartReview { uses_round: true }),
            (ReportState::Done, Running) => go(Reviewing),
            _ => refused(),
        },

        (Event::TurnUnreported | Event::TurnError(_) | Event::TurnLimit(_), Running) => blocked(turn_block(ev)),
        (Event::LimitCleared, Blocked) if t.block.as_ref().is_some_and(|b| b.limit.is_some()) => go(Running),
        (Event::GoalEnded(how), Running) => blocked(Block::new(how.clone())),

        (Event::ProcessExit, Running | WaitingYou) => go(Failed),
        (Event::ProcessExit | Event::Restart, s) if !s.is_live() => go(s),
        (Event::Restart, Running | WaitingYou) => go(Interrupted),

        (Event::Accept, Reviewing) if !t.write => go(Ready),
        (Event::Accept, Reviewing) => Err(format!(
            "{}: a write task becomes ready only through an approved review",
            t.label
        )),
        (Event::Review, Reviewing) if !t.write && !reviewing => {
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
                    blocked(Block::new("review rounds used up"))
                }
            }
        },
        (Event::BranchMoved, Reviewing) if reviewing => go(Running),
        (Event::ReviewCrashed(why), Reviewing) if reviewing => {
            if t.review_crashes == 0 {
                with(Reviewing, Effect::StartReview { uses_round: true })
            } else {
                blocked(Block::new(format!("review task crashed twice: {why}")))
            }
        }

        (Event::Merge, Ready) => {
            if t.repo.is_none() {
                Err(format!(
                    "{} is not in a git repo; nothing to merge: ask the user whether to close it (`task close`)",
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
            MergeResult::Failed(why) => blocked(Block::new(why.clone())),
        },

        (Event::Send, Blocked | Failed | Stopped | Ready | Interrupted) => go(Running),
        (Event::Send, Reviewing) => stop_review(Running),
        (Event::Send, Running | WaitingYou | Merging | Queued) => go(from),
        (Event::Stop, Running | WaitingYou | Queued) => go(Stopped),
        (Event::Stop, Reviewing) => stop_review(Stopped),
        (Event::Close, s) if s != Archived => stop_review(Archived),

        _ => refused(),
    }
}

/// The block reason of a turn that ended without a terminal report.
fn turn_block(ev: &Event) -> Block {
    match ev {
        Event::TurnError(text) => Block::new(text.clone()),
        Event::TurnLimit(limit) => Block {
            reason: match limit.kind {
                LimitKind::Quota => "quota".into(),
                LimitKind::RateLimit => "rate limit".into(),
            },
            limit: Some(limit.clone()),
        },
        _ => Block::new("ended its turn without a report"),
    }
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
        if n.from != n.to {
            t.block = n.block.clone();
            if n.to != State::Blocked {
                t.question = None;
            }
        }
        if n.to == State::Running {
            t.landed = None;
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
        t.block = Some(Block::new(reason));
        t.updated = now();
        Ok(())
    }

    /// The provider's limit cleared: its queued tasks may start again.
    pub fn lift_hold(&mut self, provider: &str) {
        self.holds.remove(provider);
    }

    /// `<dir>/board.json`; a missing file is an empty board.
    pub fn load(dir: &Path) -> io::Result<Board> {
        match std::fs::read(dir.join("board.json")) {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(io::Error::other),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Board::default()),
            Err(e) => Err(e),
        }
    }

    /// Write `board.json` atomically, then the readable `board.md`.
    pub fn save(&self, dir: &Path) -> io::Result<()> {
        let json = serde_json::to_vec_pretty(self).map_err(io::Error::other)?;
        write_atomic(&dir.join("board.json"), &json, Durability::Durable)?;
        write_atomic_no_backup(&dir.join("board.md"), self.render().as_bytes(), Durability::Fast)
    }

    /// The board snapshot for the Tasks tab: every task not archived, with the
    /// live model and tokens `live(session)` reads from its run file.
    pub fn snapshot(&self, live: impl Fn(&str) -> Option<(String, u64)>) -> Vec<TaskRow> {
        self.tasks
            .iter()
            .filter(|t| t.state != State::Archived)
            .map(|t| {
                let (model, tokens) = t
                    .session
                    .as_deref()
                    .and_then(&live)
                    .unwrap_or_else(|| (t.model.clone(), 0));
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
            let kind = match limit.kind {
                LimitKind::Quota => "quota",
                LimitKind::RateLimit => "rate limit",
            };
            out.push_str(&format!("- On hold: {provider} ({kind})\n"));
        }
        out
    }
}

/// A stable hash of a prompt's text (FNV-1a, hex): the same in every super process.
pub fn prompt_hash(text: &str) -> String {
    let h = text.bytes().fold(0xcbf2_9ce4_8422_2325u64, |h, b| {
        (h ^ u64::from(b)).wrapping_mul(0x100_0000_01b3)
    });
    format!("{h:016x}")
}

pub fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
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
        assert_eq!(spawn_state(true), Running);
        assert_eq!(spawn_state(false), Queued);
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
    fn merge_exit_rows() {
        let cases = [
            (Event::Stop, Stopped),
            (Event::TurnUnreported, Blocked),
            (Event::TurnError("e".into()), Blocked),
            (Event::ProcessExit, Failed),
            (Event::Restart, Interrupted),
            (Event::Close, Archived),
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
        for s in ALL.into_iter().filter(|s| *s != Archived) {
            assert_eq!(to(&row(s), Event::Close), Archived, "{s:?}");
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
