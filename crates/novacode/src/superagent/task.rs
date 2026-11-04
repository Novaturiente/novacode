//! The `task` tool (L10.9 "Tasks", "Controls", "Brief"): super's model starts and
//! steers tasks with it. Code owns everything around the model's call: the root and
//! trust checks, the label, the worktree, the process start, the queue, the board
//! writes and the confirmation cards for `close` and `merge`.
//!
//! Task processes are detached `novacode --bg` sessions started through a
//! [`Launcher`]; [`Procs`] is the real one. They outlive super.

use super::Tier;
use super::board::{Board, Event, Kind, MergeResult, State, Task, TaskType};
use crate::wire::{ClientCmd, Request, ServerMsg, line};
use futures_util::future::BoxFuture;
use novacode_base::paths::Paths;
use novacode_tools::ask::Asker;
use novacode_tools::{Tool, ToolCtx, ToolOutput};
use novacode_types::{Choice, Question};
use serde::Deserialize;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// What `super.*` and the session give the tool.
pub struct Config {
    pub max_running: usize,
    pub tiers: Vec<(String, Tier)>,
    /// `worktree.copy`.
    pub worktree_copy: Vec<String>,
    /// novacode's home: `trust.json` and the session files.
    pub home: PathBuf,
    /// `models.deny` and model resolution.
    pub allow: crate::agents::ModelCheck,
}

/// How a task process starts.
#[derive(Debug, Clone, PartialEq)]
pub struct Start {
    pub cwd: PathBuf,
    pub model: String,
    pub trust: bool,
    /// Resume this session instead of starting a new one.
    pub resume: Option<String>,
    pub env: Vec<(String, String)>,
}

/// Starts task processes and talks to their sockets.
pub trait Launcher: Send + Sync {
    /// Start a detached task session; returns its session id once its socket is up.
    fn start(&self, s: Start) -> BoxFuture<'_, Result<String, String>>;
    /// Send one command to a live session, as super's request.
    fn send<'a>(&'a self, session: &'a str, cmd: ClientCmd) -> BoxFuture<'a, Result<(), String>> {
        self.send_as(session, super::link::SUPER_ID, cmd)
    }
    /// Send one command with request id `id` (a prompt's id tells whose turn it starts).
    fn send_as<'a>(&'a self, session: &'a str, id: &'a str, cmd: ClientCmd) -> BoxFuture<'a, Result<(), String>>;
    fn alive(&self, session: &str) -> bool;
    /// A lasting connection to a live session's socket, for its link.
    fn link<'a>(&'a self, _session: &'a str) -> BoxFuture<'a, Result<super::link::Conn, String>> {
        Box::pin(async { Err("no task sockets".to_string()) })
    }
}

/// Super's side of its tasks: the board and everything that changes it.
pub struct Super {
    /// Canonical root folder: tasks run only in it or below.
    pub root: PathBuf,
    /// `<home>/super/<root-hash>`.
    pub dir: PathBuf,
    pub board: Mutex<Board>,
    cfg: Config,
    asker: Option<Arc<dyn Asker>>,
    pub(super) launcher: Arc<dyn Launcher>,
    /// The task links (ST13); set once after construction.
    pub links: std::sync::OnceLock<Arc<super::link::Links>>,
    /// Super's prompts per session not yet consumed by a run's end (R24).
    flights: Mutex<std::collections::HashMap<String, Flight>>,
}

/// Super's prompts to one session: questions and work never share a run (R24), so the
/// other kind waits here until the run that took the sent ones ends.
#[derive(Default)]
struct Flight {
    /// The kind of the sent ones: questions.
    question: bool,
    /// The prompt hashes sent and not yet seen consumed, oldest first.
    sent: Vec<String>,
    /// Waiting, in order: (question, text).
    held: Vec<(bool, String)>,
}

/// What a tool result adds when its prompt was held.
fn held_note(held: bool) -> &'static str {
    match held {
        true => {
            "\nIt is held: the task's run on your other prompt (a question, or work) is still going; code sends it when that run ends."
        }
        false => "",
    }
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct Args {
    action: String,
    task: Option<String>,
    request: Option<String>,
    folder: Option<String>,
    slug: Option<String>,
    write: Option<bool>,
    tier: Option<String>,
    model: Option<String>,
    #[serde(rename = "type")]
    ty: Option<String>,
    criteria: Vec<String>,
    scope: Option<String>,
    skills: Vec<String>,
    goal: bool,
    message: Option<String>,
    question: bool,
    pr: bool,
    note: Option<String>,
    all: bool,
    offset: Option<usize>,
    limit: Option<usize>,
}

/// The label's slug: 1 to 4 words of `a-z0-9` joined by `-`, at most 30 characters.
pub fn check_slug(s: &str) -> Result<(), String> {
    let words: Vec<&str> = s.split('-').collect();
    let ok = s.len() <= 30
        && (1..=4).contains(&words.len())
        && words
            .iter()
            .all(|w| !w.is_empty() && w.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit()));
    if ok {
        Ok(())
    } else {
        Err(format!(
            "bad slug `{s}`: 1 to 4 words of a-z and 0-9 joined by `-`, at most 30 characters"
        ))
    }
}

/// `type` and `write` must agree: `question` reads, `bugfix` and `change` write.
fn task_type(ty: Option<&str>, write: bool) -> Result<TaskType, String> {
    let name = ty.unwrap_or("change");
    let t = TaskType::ALL
        .into_iter()
        .find(|t| t.name() == name)
        .ok_or_else(|| format!("bad type `{name}`: bugfix, change or question"))?;
    if (t == TaskType::Question) == write {
        return Err(format!("`type: {name}` needs `write: {}`", !write));
    }
    Ok(t)
}

/// `n` of a `T<n>-…` label.
pub(super) fn number(label: &str) -> u64 {
    label
        .strip_prefix('T')
        .and_then(|r| r.split('-').next())
        .and_then(|n| n.parse().ok())
        .unwrap_or(0)
}

pub(super) fn provider(model: &str) -> &str {
    model.split('/').next().unwrap_or(model)
}

/// A task's secret for its reports.
pub(super) fn new_token() -> String {
    format!(
        "{}{}",
        novacode_base::new_id().simple(),
        novacode_base::new_id().simple()
    )
}

pub(super) fn git(dir: &Path, args: &[&str]) -> Result<String, String> {
    let o = novacode_base::git::command_in(dir)
        .args(args)
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| format!("git: {e}"))?;
    if o.status.success() {
        Ok(String::from_utf8_lossy(&o.stdout).trim_end().to_string())
    } else {
        Err(String::from_utf8_lossy(&o.stderr).trim().to_string())
    }
}

/// The main checkout of the repo `folder` is in, and `folder` relative to its own checkout.
/// Pins that repo: one nested in super's own (pinned) repo is resolved on its own, so
/// every later git call in it uses its common dir, not the outer repo's.
fn repo_of(folder: &Path) -> Option<(PathBuf, PathBuf)> {
    let (top, common) = novacode_base::git::pin_repo(folder)?;
    let common = common.as_path();
    // A bare repo's common dir is not `<main>/.git`: no main checkout to work in.
    if common.file_name()? != ".git" {
        return None;
    }
    let main = novacode_base::platform::canonicalize(common.parent()?).ok()?;
    let top = novacode_base::platform::canonicalize(top).ok()?;
    Some((main, folder.strip_prefix(&top).ok()?.to_path_buf()))
}

/// Why a folder in git has no [`repo_of`].
const NO_MAIN_CHECKOUT: &str = "is in a git submodule or a repo without a main checkout";

/// `folder` is inside a git work tree.
fn in_git(folder: &Path) -> bool {
    novacode_base::git::pin_repo(folder).is_some()
}

fn ok_choice(label: &str) -> Choice {
    Choice {
        label: label.into(),
        description: None,
        preview: None,
        recommended: false,
    }
}

/// The task preamble, in the task's system prompt (not a user message, so it stays cached).
pub fn preamble(label: &str) -> String {
    format!(
        "You are task {label}, a session started by novacode's super agent. The user talks to the super agent, \
not to you; your first message is your brief, with the user's request and its acceptance criteria.

- Call `report` when you are done, blocked or failed: `done` with `checks` (the `[proof #N]` numbers of the \
checks that prove each criterion), `blocked` with a `question` when you need the user. Reports are the only \
thing super reads; a turn that ends without one is reported as blocked.
- Your `ask_user` questions and approvals reach the user in super's window, labelled {label}.
- Edit files only inside your own worktree (or, outside git, your folder): the edit tools refuse anywhere else. \
Don't write elsewhere with `bash` either. A read task that has to write calls `enter_worktree` first.
- Commit your work before you report `done`.
- A review task (its brief says so) reports a `verdict` instead of a `state` and edits only under its \
scratch folder (its brief names it); the commit rule doesn't apply to it.
- After a restart or resume: check your folder and git before repeating anything outside it (a commit, a push, \
a message)."
    )
}

/// The brief a task gets as its first prompt. A review row holds its whole brief
/// (made by [`Super::open_review`]) in `request`.
fn brief(t: &Task) -> String {
    if t.kind == Kind::Review {
        return t.request.clone();
    }
    let mut s = format!(
        "Task {}: {} ({} task).\n\nThe user's request:\n{}\n",
        t.label,
        t.ty.name(),
        if t.write { "write" } else { "read" },
        t.request.trim()
    );
    if !t.criteria.is_empty() {
        s.push_str("\nAcceptance criteria:\n");
        for (i, c) in t.criteria.iter().enumerate() {
            s.push_str(&format!("{}. {c}\n", i + 1));
        }
    }
    if let Some(scope) = &t.scope {
        s.push_str(&format!("\nScope (the areas the change should touch): {scope}\n"));
    }
    if !t.skills.is_empty() {
        s.push_str(&format!("\nUse these skills: {}\n", t.skills.join(", ")));
    }
    match (&t.worktree, &t.branch) {
        (Some(w), Some(b)) => s.push_str(&format!(
            "\nYou work in the worktree {} on branch {b}, based on {}. Commit there; never touch the main checkout.\n",
            w.display(),
            t.base_branch.as_deref().or(t.base_commit.as_deref()).unwrap_or("HEAD")
        )),
        _ if !t.write => s.push_str("\nThis is a read task: answer, don't edit.\n"),
        _ => {}
    }
    if t.ty == TaskType::Bugfix {
        s.push_str("\nAs proof, show a test or reproduction that fails before your fix and passes after it.\n");
    }
    for m in &t.pending {
        s.push_str(&format!("\nAlso from the super agent:\n{m}\n"));
    }
    s
}

impl Super {
    pub fn new(
        root: &Path,
        dir: PathBuf,
        cfg: Config,
        asker: Option<Arc<dyn Asker>>,
        launcher: Arc<dyn Launcher>,
    ) -> std::io::Result<Self> {
        std::fs::create_dir_all(&dir)?;
        let board = Board::load(&dir)?;
        super::review::sweep_scratch(&dir, &board);
        // A restarted super pins its tasks' repos again (nested repos included).
        for main in board.tasks.iter().filter_map(|t| t.repo.as_deref()) {
            novacode_base::git::pin_repo(main);
        }
        Ok(Super {
            root: novacode_base::platform::canonicalize(root)?,
            board: Mutex::new(board),
            dir,
            cfg,
            asker,
            launcher,
            links: std::sync::OnceLock::new(),
            flights: Mutex::default(),
        })
    }

    /// novacode's home (goal files, session files).
    pub fn home(&self) -> &Path {
        &self.cfg.home
    }

    pub(super) fn cfg(&self) -> &Config {
        &self.cfg
    }

    /// Change the board and save it.
    pub(super) fn edit<R>(&self, f: impl FnOnce(&mut Board) -> R) -> Result<R, String> {
        let mut b = self.board.lock().unwrap();
        let r = f(&mut b);
        b.save(&self.dir).map_err(|e| format!("couldn't save the board: {e}"))?;
        Ok(r)
    }

    pub(super) fn row(&self, id: &str) -> Result<Task, String> {
        self.board
            .lock()
            .unwrap()
            .task(id)
            .cloned()
            .ok_or_else(|| format!("no task {id}"))
    }

    /// A task by its label, or by `T<n>`.
    fn find(&self, name: &str) -> Result<Task, String> {
        let b = self.board.lock().unwrap();
        b.tasks
            .iter()
            .find(|t| t.label == name || t.id == name)
            .or_else(|| {
                let n = name.strip_prefix('T')?.parse::<u64>().ok()?;
                b.tasks.iter().find(|t| number(&t.label) == n)
            })
            .cloned()
            .ok_or_else(|| format!("no task {name}; `task list {{all: true}}` shows every task"))
    }

    /// Whether `ev` is possible for `id` now, without changing anything.
    fn check(&self, id: &str, ev: Event) -> Result<(), String> {
        self.board.lock().unwrap().clone().apply(id, ev).map(|_| ())
    }

    pub async fn run(&self, args: Value) -> Result<String, String> {
        let a: Args = serde_json::from_value(args).map_err(|e| format!("invalid arguments: {e}"))?;
        if a.action == "spawn" {
            return self.spawn(a).await;
        }
        if a.action == "list" {
            return Ok(self.list(a.all));
        }
        if a.action == "info" {
            return self.info(a.folder.as_deref().ok_or("`folder` is missing")?);
        }
        let t = self.find(a.task.as_deref().ok_or("`task` is missing")?)?;
        match a.action.as_str() {
            "read" => self.read(&t, a.offset.unwrap_or(0), a.limit.unwrap_or(200)),
            "send" if a.question => self.ask(&t, a.message.as_deref().ok_or("`message` is missing")?).await,
            "send" => self.send(&t, a.message.as_deref().ok_or("`message` is missing")?).await,
            "stop" => self.stop(&t).await,
            "close" => self.close(&t).await,
            "merge" => self.merge(&t, a.pr).await,
            "accept" => self.accept(&t).await,
            "review" => {
                self.check(&t.id, Event::Review)?;
                // A code-made block (R23): no report, so code takes the facts from its commits.
                let unreported = (t.state == State::Blocked).then(|| commits_done(&t)).transpose()?;
                self.edit(|b| {
                    let n = b.apply(&t.id, Event::Review)?;
                    if let (Some(done), Some(row)) = (unreported, b.tasks.iter_mut().find(|r| r.id == t.id)) {
                        row.done = Some(done);
                        row.summary = Some("no report; reviewed from its commits".into());
                    }
                    Ok::<_, String>(n)
                })??;
                let review = self.open_review(&t.id, a.note.as_deref())?;
                let mut out = format!("{} is reviewing; its review task is {review}.", t.label);
                for n in self.pump().await {
                    out += &format!("\n{n}");
                }
                Ok(out)
            }
            other => Err(format!(
                "unknown action `{other}`: spawn, info, send, stop, close, accept, merge, review, list or read"
            )),
        }
    }

    async fn spawn(&self, a: Args) -> Result<String, String> {
        let request = a
            .request
            .filter(|r| !r.trim().is_empty())
            .ok_or("`request` is missing")?;
        let slug = a.slug.ok_or("`slug` is missing")?;
        check_slug(&slug)?;
        let write = a.write.ok_or("`write` is missing")?;
        let ty = task_type(a.ty.as_deref(), write)?;
        if a.criteria.is_empty() || a.criteria.len() > 5 {
            return Err("give 1 to 5 checkable acceptance `criteria`".into());
        }
        let folder = self.in_root(a.folder.as_deref().ok_or("`folder` is missing")?)?;
        let (tier, model) = match (a.tier, a.model) {
            (Some(_), Some(_)) => return Err("give `tier` or `model`, not both".into()),
            (None, Some(m)) => (None, m),
            (tier, None) => {
                let name = tier.unwrap_or_else(|| "standard".into());
                let t = self.cfg.tiers.iter().find(|(n, _)| *n == name).ok_or_else(|| {
                    let names: Vec<&str> = self.cfg.tiers.iter().map(|(n, _)| n.as_str()).collect();
                    format!("no tier `{name}`: {}", names.join(", "))
                })?;
                (Some(name), t.1.model.clone())
            }
        };
        (self.cfg.allow)(&model)?;
        let repo = repo_of(&folder);
        if write && repo.is_none() && in_git(&folder) {
            return Err(format!(
                "`{}` {NO_MAIN_CHECKOUT}: a write task needs its own worktree, made from a main checkout. \
                 Spawn it as a read task, or in the repo's own clone.",
                folder.display()
            ));
        }
        let next_label = |b: &Board| {
            let n = b.tasks.iter().map(|t| number(&t.label)).max().unwrap_or(0) + 1;
            format!("T{n}-{slug}")
        };
        // The card names the label the task will likely get; the number is taken when
        // the row is added (a review may take one while the card is open).
        let shown = next_label(&self.board.lock().unwrap());
        let trusted = self.trust(&shown, &folder, repo.as_ref()).await?;
        let now = super::board::now();
        let mut row = Task {
            kind: Kind::Work,
            ty,
            folder: folder.strip_prefix(&self.root).unwrap_or(Path::new("")).to_path_buf(),
            repo: repo.map(|(main, _)| main),
            tier,
            model,
            trusted,
            token: new_token(),
            write,
            criteria: a.criteria,
            scope: a.scope,
            request,
            skills: a.skills,
            goal: a.goal,
            state: State::Queued,
            started: now,
            updated: now,
            ..Task::default()
        };
        let label = self.edit(|b| {
            let label = next_label(b);
            row.id = label.clone();
            row.label = label.clone();
            b.tasks.push(row);
            label
        })?;
        let notes = self.pump().await;
        let t = self.row(&label)?;
        let mut out = match t.state {
            State::Queued => {
                let mut s = format!("{label} is queued: {}.", self.why_queued(&t));
                let on = t
                    .repo
                    .as_ref()
                    .filter(|_| t.write)
                    .and_then(|main| git(main, &["branch", "--show-current"]).ok())
                    .filter(|b| !b.is_empty());
                if let Some(b) = on {
                    s += &format!(" Its worktree branches from {b} (the main checkout's branch) when it starts.");
                }
                s
            }
            State::Failed => format!("{label} failed to start: {}", t.summary.unwrap_or_default()),
            s => {
                let mut s = format!("Started {label} ({}, {}).", s.name(), t.model);
                if let (Some(w), Some(b)) = (&t.worktree, &t.branch) {
                    s += &format!(
                        " Worktree {} on branch {b}, from {}.",
                        w.display(),
                        t.base_branch.as_deref().or(t.base_commit.as_deref()).unwrap_or("HEAD")
                    );
                }
                s
            }
        };
        for n in notes.into_iter().filter(|n| !n.starts_with(&label)) {
            out += &format!("\n{n}");
        }
        Ok(out)
    }

    /// `task info {folder}`, read-only: what a task there would get (git or not, the branch
    /// a write task's worktree starts from, trust), for super to show with the criteria.
    fn info(&self, folder: &str) -> Result<String, String> {
        use novacode_core::trust::{self, TrustStore};
        let folder = self.in_root(folder)?;
        let repo = repo_of(&folder);
        let mut s = format!("{}: ", folder.display());
        s += &match &repo {
            Some((main, _)) => match git(main, &["branch", "--show-current"]).ok().filter(|b| !b.is_empty()) {
                Some(b) => format!(
                    "git repo; its main checkout {} is on branch {b}, so a write task's worktree branches from {b}.",
                    main.display()
                ),
                None => format!(
                    "git repo; its main checkout {} is on a detached HEAD: a write task's worktree starts from that \
                     commit and can't be merged locally.",
                    main.display()
                ),
            },
            None if in_git(&folder) => format!("it {NO_MAIN_CHECKOUT}: only read tasks run here."),
            None => "not in git: a write task edits the folder in place, one at a time.".to_string(),
        };
        let at = repo.as_ref().map_or(folder.clone(), |(main, rel)| main.join(rel));
        let (state, _) = TrustStore::new(self.cfg.home.join("trust.json")).check(&at);
        s += match state {
            trust::State::Trusted => " Trust: trusted.",
            _ if !trust::has_project_config(&at) => " Trust: no project config, nothing to trust.",
            trust::State::Changed(_) => {
                " Trust: its project config changed since it was trusted; `task spawn` asks the user on a card."
            }
            _ => " Trust: not trusted (it has project config); `task spawn` asks the user on a card.",
        };
        Ok(s)
    }

    /// The task folder, resolved (symlinks followed); refused outside the root.
    fn in_root(&self, folder: &str) -> Result<PathBuf, String> {
        let p = Path::new(folder);
        let p = if p.is_absolute() {
            p.to_path_buf()
        } else {
            self.root.join(p)
        };
        let real = novacode_base::platform::canonicalize(&p).map_err(|e| format!("folder `{folder}`: {e}"))?;
        if !real.starts_with(&self.root) {
            return Err(format!(
                "`{}` is outside the super root {}: tasks run only there",
                real.display(),
                self.root.display()
            ));
        }
        if !real.is_dir() {
            return Err(format!("`{}` is not a folder", real.display()));
        }
        Ok(real)
    }

    /// Trust for the task's project, checked at its main checkout (a new worktree has no
    /// entry of its own); asks the user on a card when it is untrusted or changed.
    async fn trust(&self, label: &str, folder: &Path, repo: Option<&(PathBuf, PathBuf)>) -> Result<bool, String> {
        use novacode_core::trust::{self, TrustStore};
        let at = repo.map_or(folder.to_path_buf(), |(main, rel)| main.join(rel));
        let store = TrustStore::new(self.cfg.home.join("trust.json"));
        let (state, fp) = store.check(&at);
        if state == trust::State::Trusted {
            return Ok(true);
        }
        if !trust::has_project_config(&at) {
            return Ok(false);
        }
        let Some(asker) = &self.asker else { return Ok(false) };
        let base = repo
            .and_then(|(main, _)| git(main, &["branch", "--show-current"]).ok())
            .filter(|b| !b.is_empty());
        let mut q = match &state {
            trust::State::Changed(c) => format!(
                "{label} would run in {}, whose project config changed since you trusted it ({}).",
                at.display(),
                c.join(", ")
            ),
            _ => format!(
                "{label} would run in {}, which has project config (settings, MCP servers, skills, hooks).",
                at.display()
            ),
        };
        if let Some(b) = base {
            q += &format!(" Its worktree branches from {b}.");
        }
        q += " Trusting it lets that config change settings and run code. Trust it?";
        let (this, once, never) = ("Trust this folder", "Trust for this task only", "Don't trust");
        let question = Question {
            question: q,
            header: Some(format!("{label}: trust")),
            options: vec![ok_choice(this), ok_choice(once), ok_choice(never)],
            multi: false,
        };
        let pick = asker
            .ask(vec![question])
            .await
            .and_then(|r| r.into_iter().next())
            .and_then(|r| r.chosen.into_iter().next());
        let err = |e: std::io::Error| format!("couldn't save the trust decision: {e}");
        Ok(match pick.as_deref() {
            Some(p) if p == this => {
                store.trust_with(&at, &at, fp).map_err(err)?;
                true
            }
            Some(p) if p == once => true,
            Some(p) if p == never => {
                store.set(&at, false).map_err(err)?;
                false
            }
            _ => false,
        })
    }

    fn why_queued(&self, t: &Task) -> String {
        let b = self.board.lock().unwrap();
        if let Some(l) = b.holds.get(provider(&t.model)) {
            return format!("{} is on hold ({:?})", l.provider, l.kind);
        }
        if let Some(o) = folder_holder(&b, t) {
            return format!("{} writes in that folder (no git), one at a time", o.label);
        }
        format!("{} tasks run already (super.maxRunning)", self.cfg.max_running)
    }

    /// Start queued tasks while slots are free: reviews first, then the oldest; tasks on
    /// a provider on hold, and non-git write tasks whose folder is taken, wait.
    /// Returns a note per task that failed to start.
    pub async fn pump(&self) -> Vec<String> {
        let mut notes = Vec::new();
        loop {
            // Pick and take the slot under one lock: the tick and the tool both pump.
            let took = {
                let mut b = self.board.lock().unwrap();
                pick(&b, self.cfg.max_running).map(|id| {
                    b.apply(&id, Event::SlotFree)?;
                    b.save(&self.dir).map_err(|e| format!("couldn't save the board: {e}"))?;
                    Ok::<_, String>(id)
                })
            };
            let id = match took {
                Some(Ok(id)) => id,
                None => break,
                Some(Err(e)) => {
                    notes.push(e);
                    break;
                }
            };
            if let Err(e) = self.start(&id).await {
                notes.push(format!("{id} failed to start: {e}"));
            }
        }
        notes
    }

    /// Start (or resume) a task's process; a new one gets its brief. On failure the row
    /// goes through the table as a process exit: a live one (a merge too, whose lock is
    /// released) becomes `failed`; a ready, reviewing or blocked one keeps its state.
    async fn start(&self, id: &str) -> Result<(), String> {
        let r = self.start_inner(id).await;
        if let Err(e) = &r {
            let row = self.row(id)?;
            if let (Kind::Review, Some(of)) = (row.kind, &row.reviewer_of) {
                // The review's model has no login, doesn't resolve, …: not a crash, not retried.
                self.retire_review(id);
                self.edit(|b| b.apply(of, Event::ReviewCantStart(format!("its review can't start: {e}"))))??;
                if let Some(l) = self.links.get() {
                    l.wake(format!("{of} is blocked: its review task {id} can't start: {e}"));
                }
                return r;
            }
            let n = self.edit(|b| {
                let n = b.apply(id, Event::ProcessExit).ok()?;
                if n.to == State::Failed
                    && let Some(t) = b.tasks.iter_mut().find(|t| t.id == id)
                {
                    t.summary = Some(format!("didn't start: {e}"));
                }
                Some(n)
            })?;
            if let Some(n) = n {
                self.merge_exit(id, &n);
            }
        }
        r
    }

    async fn start_inner(&self, id: &str) -> Result<(), String> {
        let mut t = self.row(id)?;
        let resume = t.session.clone();
        if t.cwd.is_none() {
            let folder = self.root.join(&t.folder);
            let cwd = match (&t.repo, t.write) {
                _ if t.kind == Kind::Review => self.review_cwd(&t)?,
                (Some(main), true) => {
                    let rel = repo_of(&folder).map(|(_, rel)| rel).unwrap_or_default();
                    t.base_branch = git(main, &["branch", "--show-current"]).ok().filter(|b| !b.is_empty());
                    t.base_commit = Some(git(main, &["rev-parse", "HEAD"])?);
                    let (wt, _) = novacode_tools::worktree::create_new(main, &t.label, &self.cfg.worktree_copy)?;
                    let name = wt.file_name().unwrap_or_default().to_string_lossy();
                    t.branch = Some(format!("worktree-{name}"));
                    t.worktree = Some(wt.clone());
                    let cwd = wt.join(rel);
                    std::fs::create_dir_all(&cwd).map_err(|e| e.to_string())?;
                    cwd
                }
                _ => folder,
            };
            t.cwd = Some(cwd);
            let t = t.clone();
            self.edit(|b| {
                if let Some(row) = b.tasks.iter_mut().find(|r| r.id == t.id) {
                    *row = t;
                }
            })?;
        }
        let cwd = t.cwd.clone().unwrap_or_default();
        // Its own worktree (other worktrees of the repo stay refused), else its folder;
        // a review task only its scratch folder.
        let rule = match t.kind {
            Kind::Review => format!("review:{}", super::review::scratch(&self.dir, &t).display()),
            Kind::Work => format!(
                "worktree:{}",
                t.worktree
                    .clone()
                    .unwrap_or_else(|| self.root.join(&t.folder))
                    .display()
            ),
        };
        let env = vec![
            ("NOVACODE_TASK".to_string(), t.id.clone()),
            ("NOVACODE_TASK_TOKEN".to_string(), t.token.clone()),
            (
                "NOVACODE_SUPER_DIR".to_string(),
                self.dir.to_string_lossy().into_owned(),
            ),
            (novacode_core::policy::EditRule::ENV.to_string(), rule),
        ];
        let session = self
            .launcher
            .start(Start {
                cwd,
                model: t.model.clone(),
                trust: t.trusted,
                resume: resume.clone(),
                env,
            })
            .await?;
        let pending = std::mem::take(&mut t.pending);
        self.edit(|b| {
            if let Some(row) = b.tasks.iter_mut().find(|r| r.id == id) {
                row.session = Some(session.clone());
                if resume.is_none() {
                    row.pending.clear();
                }
            }
        })?;
        if let Some(l) = self.links.get() {
            l.open(id, &session);
        }
        if resume.is_none() {
            t.pending = pending;
            // Open-ended work: the criteria the user approved become the task's goal. `set_goal`
            // starts no run; the brief does.
            if t.goal {
                let goal = ClientCmd::SetGoal {
                    objective: t.request.trim().to_string(),
                    criteria: t.criteria.clone(),
                };
                self.launcher.send(&session, goal).await?;
            }
            self.prompt(&session, brief(&t)).await?;
        }
        Ok(())
    }

    /// A goal task whose goal is paused (a stop paused it) continues it: the message means continue.
    async fn resume_goal(&self, id: &str, session: &str) -> Result<(), String> {
        let goal = self.row(id)?.goal;
        if goal && super::link::Goal::read(&self.cfg.home, session) == super::link::Goal::Paused {
            let resume = ClientCmd::Prompt {
                text: "/goal resume".into(),
                raw: false,
            };
            self.launcher.send(session, resume).await?;
        }
        Ok(())
    }

    /// After an event that left `merging` other than by a merge report (its lock is
    /// released): a merge left half-done blocks the task. Returns the block reason.
    pub(super) fn merge_exit(&self, id: &str, n: &super::board::Next) -> Option<String> {
        if !n.effects.contains(&super::board::Effect::CheckMergeLeft) {
            return None;
        }
        let reason = format!("merge left half-done: {}", half_done(&self.row(id).ok()?)?);
        match self.edit(|b| b.merge_left(id, &reason)) {
            Ok(Ok(())) => Some(reason),
            _ => None,
        }
    }

    /// A report said `write` (a read task that called `enter_worktree`): the row becomes a
    /// write task with the worktree, branch and base a write task's start records, so
    /// review, merge and close work; a `done` already recorded gets its changed files.
    pub(super) fn became_write(&self, id: &str, branch: Option<&str>) -> Result<(), String> {
        let t = self.row(id)?;
        if t.write {
            return Ok(());
        }
        let found = match (&t.repo, branch) {
            (Some(main), Some(branch)) if t.worktree.is_none() => worktree_of(main, branch).map(|wt| {
                let base_branch = git(main, &["branch", "--show-current"]).ok().filter(|b| !b.is_empty());
                let base_commit = git(main, &["merge-base", branch, base_branch.as_deref().unwrap_or("HEAD")]).ok();
                let files = match (&base_commit, t.done.as_ref().and_then(|d| d.commit.as_ref())) {
                    (Some(base), Some(commit)) if t.done.as_ref().is_some_and(|d| d.files.is_empty()) => {
                        let copies = novacode_tools::worktree::copies(&wt);
                        git(main, &["diff", "--name-only", &format!("{base}...{commit}")])
                            .unwrap_or_default()
                            .lines()
                            .filter(|f| !f.is_empty() && !copies.iter().any(|c| f.starts_with(c.as_str())))
                            .map(String::from)
                            .collect()
                    }
                    _ => Vec::new(),
                };
                (wt, branch.to_string(), base_branch, base_commit, files)
            }),
            _ => None,
        };
        self.edit(|b| {
            let Some(row) = b.tasks.iter_mut().find(|r| r.id == id) else {
                return;
            };
            row.write = true;
            if let Some((wt, branch, base_branch, base_commit, files)) = found {
                row.worktree = Some(wt);
                row.branch = Some(branch);
                row.base_branch = base_branch;
                row.base_commit = base_commit;
                if let Some(d) = row.done.as_mut().filter(|_| !files.is_empty()) {
                    d.files = files;
                }
            }
        })
    }

    /// A raw prompt: a brief or message starting with `!` or `/` is not a command.
    /// `true` when it waits for the task's run on a question to end.
    async fn prompt(&self, session: &str, text: String) -> Result<bool, String> {
        self.prompt_as(session, super::link::SUPER_ID, text).await
    }

    /// [`prompt`](Self::prompt) with request id `id`; held (`true`) while a prompt of the
    /// other kind (question or work) is sent and its run has not ended.
    async fn prompt_as(&self, session: &str, id: &str, text: String) -> Result<bool, String> {
        let question = id == super::link::SUPER_QUESTION_ID;
        {
            let mut flights = self.flights.lock().unwrap();
            let f = flights.entry(session.to_string()).or_default();
            if !f.held.is_empty() || (!f.sent.is_empty() && f.question != question) {
                f.held.push((question, text));
                return Ok(true);
            }
            f.question = question;
            f.sent.push(super::board::prompt_hash(&text));
        }
        self.send_prompt(session, id, text).await.map(|_| false)
    }

    async fn send_prompt(&self, session: &str, id: &str, text: String) -> Result<(), String> {
        let hash = super::board::prompt_hash(&text);
        if let Err(e) = self
            .launcher
            .send_as(session, id, ClientCmd::Prompt { text, raw: true })
            .await
        {
            if let Some(f) = self.flights.lock().unwrap().get_mut(session)
                && let Some(i) = f.sent.iter().rposition(|h| *h == hash)
            {
                f.sent.remove(i);
            }
            return Err(e);
        }
        self.edit(|b| {
            if let Some(row) = b.tasks.iter_mut().find(|r| r.session.as_deref() == Some(session)) {
                row.last_prompt = Some(hash);
            }
        })
    }

    /// A run of `session` ended having consumed `consumed` of super's prompts: once none is
    /// left, the held ones of the next kind are sent.
    pub(super) async fn ran(&self, session: &str, consumed: usize) {
        let (question, next) = {
            let mut flights = self.flights.lock().unwrap();
            let Some(f) = flights.get_mut(session) else { return };
            f.sent.drain(..consumed.min(f.sent.len()));
            let Some(&(question, _)) = f.held.first().filter(|_| f.sent.is_empty()) else {
                return;
            };
            let n = f.held.iter().take_while(|h| h.0 == question).count();
            f.question = question;
            let next: Vec<_> = f.held.drain(..n).map(|h| h.1).collect();
            f.sent = next.iter().map(|t| super::board::prompt_hash(t)).collect();
            (question, next)
        };
        let id = match question {
            true => super::link::SUPER_QUESTION_ID,
            false => super::link::SUPER_ID,
        };
        for text in next {
            if let Err(e) = self.send_prompt(session, id, text).await {
                tracing::warn!(session, %e, "super: a held prompt couldn't be sent");
            }
        }
    }

    /// A reconnect found `session` not busy, its history's user prompts hashing to `seen`:
    /// the in-flight prompts up to the last one seen were consumed; later ones stay in
    /// flight (they will start a run).
    pub(super) async fn caught_up(&self, session: &str, seen: &[String]) {
        let consumed = {
            let flights = self.flights.lock().unwrap();
            let Some(f) = flights.get(session) else { return };
            seen.iter()
                .rev()
                .find_map(|h| f.sent.iter().rposition(|s| s == h))
                .map_or(0, |i| i + 1)
        };
        self.ran(session, consumed).await;
    }

    /// The session's run was stopped or its process is gone: nothing is in flight and
    /// nothing held is sent.
    pub(super) fn grounded(&self, session: &str) {
        self.flights.lock().unwrap().remove(session);
    }

    /// Send `text` to the task, resuming its session first when its process is gone.
    /// `true` when it waits for the task's run on a question to end.
    pub(super) async fn deliver(&self, id: &str, text: String) -> Result<bool, String> {
        let t = self.row(id)?;
        match &t.session {
            Some(s) if self.launcher.alive(s) => {
                self.resume_goal(id, s).await?;
                self.prompt(s, text).await
            }
            Some(_) => {
                self.start(id).await?;
                let s = self.row(id)?.session.unwrap_or_default();
                self.resume_goal(id, &s).await?;
                self.prompt(&s, text).await
            }
            None => {
                self.edit(|b| {
                    if let Some(row) = b.tasks.iter_mut().find(|r| r.id == id) {
                        row.pending.push(text);
                    }
                })?;
                self.start(id).await.map(|_| false)
            }
        }
    }

    /// The review task of a reviewed row stops with it (`send` or `stop` on `reviewing`).
    async fn stop_review(&self, reviewer: Option<String>) {
        let Some(r) = reviewer.and_then(|r| self.row(&r).ok()) else {
            return;
        };
        if let Some(s) = r.session.as_deref().filter(|s| self.launcher.alive(s)) {
            let _ = self.launcher.send(s, ClientCmd::Shutdown).await;
        }
        self.retire_review(&r.id);
    }

    /// `task accept`: a read task in `reviewing` or blocked by code, or a write task whose
    /// review rounds are used up, after the user's OK on a card made by code.
    async fn accept(&self, t: &Task) -> Result<String, String> {
        self.check(&t.id, Event::Accept)?;
        if t.state == State::Blocked && t.write {
            let q = format!(
                "Accept {} without an approved review? Its review rounds are used up; it becomes ready, \
                 marked accepted without review, and can be merged.",
                t.label
            );
            if !self.confirm(format!("{}: accept", t.label), q, "Accept").await? {
                return Ok(format!("The user declined: {} stays blocked.", t.label));
            }
        }
        let n = self.edit(|b| b.apply(&t.id, Event::Accept))??;
        if n.effects.contains(&super::board::Effect::StopReview) {
            self.stop_review(t.reviewer.clone()).await;
        }
        Ok(format!("{} is ready.", t.label))
    }

    async fn send(&self, t: &Task, message: &str) -> Result<String, String> {
        let follow_up = |b: &mut Board| {
            if let Some(row) = b.tasks.iter_mut().find(|r| r.id == t.id) {
                row.follow_ups.push(message.to_string());
            }
        };
        if t.state == State::Queued {
            self.edit(|b| {
                b.apply(&t.id, Event::Send)?;
                follow_up(b);
                if let Some(row) = b.tasks.iter_mut().find(|r| r.id == t.id) {
                    row.pending.push(message.to_string());
                }
                Ok::<_, String>(())
            })??;
            return Ok(format!("Added to {}'s brief; it is still queued.", t.label));
        }
        let (n, reviewer) = self.edit(|b| {
            let r = reviewer(b, &t.id);
            let n = b.apply(&t.id, Event::Send)?;
            follow_up(b);
            Ok::<_, String>((n, r))
        })??;
        if n.effects.contains(&super::board::Effect::StopReview) {
            self.stop_review(reviewer).await;
        }
        let held = self.deliver(&t.id, message.to_string()).await?;
        Ok(format!("Sent to {} ({}).{}", t.label, n.to.name(), held_note(held)))
    }

    /// `send` with `question: true`: the task answers in text; its board state and any
    /// review stay as they are, and its reply wakes super (the link's watcher).
    async fn ask(&self, t: &Task, message: &str) -> Result<String, String> {
        self.check(&t.id, Event::Send)?;
        if t.state == State::Queued {
            return Err(format!("{} is queued: it has no session to answer yet", t.label));
        }
        let text = super::link::question_prompt(message);
        let s = match &t.session {
            Some(s) if self.launcher.alive(s) => s.clone(),
            Some(_) => {
                self.start(&t.id).await?;
                self.row(&t.id)?.session.unwrap_or_default()
            }
            None => return Err(format!("{} has no session to answer", t.label)),
        };
        let held = self.prompt_as(&s, super::link::SUPER_QUESTION_ID, text).await?;
        Ok(format!(
            "Asked {} (still {}); its reply wakes you when its turn ends.{}",
            t.label,
            t.state.name(),
            held_note(held)
        ))
    }

    async fn stop(&self, t: &Task) -> Result<String, String> {
        let (n, reviewer) = self.edit(|b| {
            let r = reviewer(b, &t.id);
            b.apply(&t.id, Event::Stop).map(|n| (n, r))
        })??;
        if n.from == State::Queued {
            return Ok(format!("{} is out of the queue (stopped).", t.label));
        }
        if n.effects.contains(&super::board::Effect::StopReview) {
            self.stop_review(reviewer).await;
        }
        if let Some(s) = t.session.as_deref() {
            self.grounded(s);
        }
        if let Some(s) = t.session.as_deref().filter(|s| self.launcher.alive(s)) {
            self.launcher.send(s, ClientCmd::Abort { drop_queued: true }).await?;
        }
        let first = match self.merge_exit(&t.id, &n) {
            Some(why) => format!(
                "Stopped {}'s merge; the merge lock is released, but it is blocked: {why}.",
                t.label
            ),
            None if n.from == State::Merging => {
                format!(
                    "Stopped {}'s merge; the merge lock is released. Its session stays.",
                    t.label
                )
            }
            None => format!("Stopped {}; its session stays.", t.label),
        };
        let notes = self.pump().await;
        Ok(std::iter::once(first).chain(notes).collect::<Vec<_>>().join("\n"))
    }

    /// Whether a task's branch is merged into its own base (not into `HEAD`, as `git branch -d` checks).
    fn merged(t: &Task) -> Option<bool> {
        let (main, branch) = (t.repo.as_ref()?, t.branch.as_ref()?);
        let base = t.base_branch.as_ref().or(t.base_commit.as_ref())?;
        Some(git(main, &["merge-base", "--is-ancestor", branch, base]).is_ok())
    }

    /// A confirmation card made by code: true only when the user picks `ok`.
    async fn confirm(&self, header: String, question: String, ok: &str) -> Result<bool, String> {
        Ok(self.choose(header, question, &[ok, "Cancel"]).await?.as_deref() == Some(ok))
    }

    /// A card made by code: the choice the user picked, `None` when dismissed.
    async fn choose(&self, header: String, question: String, choices: &[&str]) -> Result<Option<String>, String> {
        let asker = self.asker.as_ref().ok_or("no window to confirm in")?;
        let q = Question {
            question,
            header: Some(header),
            options: choices.iter().map(|c| ok_choice(c)).collect(),
            multi: false,
        };
        Ok(asker
            .ask(vec![q])
            .await
            .and_then(|r| r.into_iter().next())
            .and_then(|r| r.chosen.into_iter().find(|c| choices.contains(&c.as_str()))))
    }

    async fn close(&self, t: &Task) -> Result<String, String> {
        if t.state == State::Merging {
            return Err(format!(
                "{} is merging: wait for the merge result before closing it (or `task stop` it to end the merge)",
                t.label
            ));
        }
        self.check(&t.id, Event::Close)?;
        let merged = Self::merged(t);
        let mut q = format!("Close {}? Its session ends; the session file is kept.", t.label);
        let (close, close_rm) = ("Close", "Close and remove worktree");
        let mut choices = vec![close, "Cancel"];
        if let (Some(w), Some(b)) = (&t.worktree, &t.branch) {
            let base = t
                .base_branch
                .as_deref()
                .or(t.base_commit.as_deref())
                .unwrap_or("its base");
            q += &match merged {
                Some(true) => format!(
                    " Branch {b} is merged into {base}: its worktree {} is removed.",
                    w.display()
                ),
                _ => {
                    choices.insert(1, close_rm);
                    format!(
                        " Branch {b} is NOT merged into {base}: its worktree {} is kept, unless you \
                         remove it (the branch stays either way).",
                        w.display()
                    )
                }
            };
        }
        let pick = self.choose(format!("{}: close", t.label), q, &choices).await?;
        let remove = match pick.as_deref() {
            Some(p) if p == close => merged == Some(true),
            Some(p) if p == close_rm => true,
            _ => return Ok(format!("The user declined: {} stays open.", t.label)),
        };
        // The review running now, not the one seen before the card.
        let (n, reviewer) = self.edit(|b| {
            let r = reviewer(b, &t.id);
            b.apply(&t.id, Event::Close).map(|n| (n, r))
        })??;
        if n.effects.contains(&super::board::Effect::StopReview) {
            self.stop_review(reviewer).await;
        }
        if let Some(s) = t.session.as_deref().filter(|s| self.launcher.alive(s)) {
            let _ = self.launcher.send(s, ClientCmd::Shutdown).await;
            if remove {
                for _ in 0..100 {
                    if !self.launcher.alive(s) {
                        break;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                }
            }
        }
        if let Some(why) = self.merge_exit(&t.id, &n) {
            return Ok(format!(
                "{}'s session ended, but it is blocked, not closed: {why}. Its worktree is kept.",
                t.label
            ));
        }
        let mut out = format!("Closed {}.", t.label);
        if let Some(w) = &t.worktree {
            match (remove, novacode_tools::worktree::here(w)) {
                (true, Ok(h)) => {
                    // A daemon the task started (codegraph's) outlives it and keeps writing there.
                    let dir = w.clone();
                    let _ = tokio::task::spawn_blocking(move || novacode_tools::process::stop_inside(&dir)).await;
                    out += &novacode_tools::worktree::remove(&h)
                }
                (true, Err(e)) => out += &format!(" Worktree not removed: {e}."),
                _ => out += &format!(" Worktree {} kept (not merged).", w.display()),
            }
        }
        for n in self.pump().await {
            out += &format!("\n{n}");
        }
        Ok(out)
    }

    async fn merge(&self, t: &Task, pr: bool) -> Result<String, String> {
        self.check(&t.id, Event::Merge)?;
        let (main, branch) = (
            t.repo.clone().unwrap_or_default(),
            t.branch.clone().unwrap_or_else(|| "its branch".into()),
        );
        let base = t
            .base_branch
            .clone()
            .or(t.base_commit.clone())
            .unwrap_or_else(|| "its base".into());
        let q = match pr {
            true => format!(
                "Merge {}? Branch {branch} is rebased on {base}, pushed, and a pull request against {base} is opened.",
                t.label
            ),
            false => format!(
                "Merge {}? Branch {branch} is rebased on {base} and merged into {base} in the main checkout {}.",
                t.label,
                main.display()
            ),
        };
        if !self.confirm(format!("{}: merge", t.label), q, "Merge").await? {
            return Ok(format!("The user declined: {} is not merged.", t.label));
        }
        self.edit(|b| b.apply(&t.id, Event::Merge))??;
        // The user's checkout is never switched or merged into while dirty: a local merge
        // needs it clean and on the task's base.
        let refused = match &t.base_branch {
            None => Some(format!("its base is commit {base}, not a branch")),
            Some(want) if !pr => {
                let on = git(&main, &["branch", "--show-current"]).unwrap_or_default();
                if on != *want {
                    let on = if on.is_empty() { "a detached HEAD" } else { &on };
                    Some(format!("main checkout is on {on}, not {want}"))
                } else {
                    changes(&main).then(|| "main checkout has uncommitted changes".to_string())
                }
            }
            Some(_) => None,
        };
        if let Some(why) = refused {
            self.edit(|b| b.apply(&t.id, Event::MergeReport(MergeResult::Failed(why.clone()))))??;
            return Ok(format!(
                "{} merge: failed (now blocked): {why}. The main checkout was not touched; the merge lock is released.",
                t.label
            ));
        }
        let tip = t
            .base_branch
            .as_ref()
            .and_then(|b| git(&main, &["rev-parse", &format!("refs/heads/{b}")]).ok());
        let recheck = match (tip, &t.base_commit) {
            (Some(tip), Some(old)) if tip != *old => format!(
                "{base} moved since your work started ({} → {}): after the rebase, re-run your checks and merge only if they pass.",
                &old[..old.len().min(8)],
                &tip[..tip.len().min(8)]
            ),
            _ => "If the rebase moved your branch, re-run your checks before you merge.".to_string(),
        };
        let how = match pr {
            true => format!("push it and open a pull request against {base}, then report `merge: pr-opened`"),
            false => format!(
                "merge it into {base} in the main checkout {} (fast-forward), then report `merge: merged`",
                main.display()
            ),
        };
        let text = format!(
            "The user approved merging. Rebase {branch} on {base}. {recheck} Then {how}. \
If anything fails (a conflict, uncommitted changes to tracked files in the main checkout, a failed check), report \
`merge: failed` with the reason. Untracked files (build output, caches) are not a reason to stop: git refuses by \
itself if the merge would overwrite one."
        );
        let held = self.deliver(&t.id, text).await?;
        Ok(format!("{} is merging.{}", t.label, held_note(held)))
    }

    fn list(&self, all: bool) -> String {
        let b = self.board.lock().unwrap();
        let now = super::board::now();
        let rows: Vec<String> = b
            .tasks
            .iter()
            .filter(|t| all || t.state != State::Archived)
            .map(|t| {
                let state = t.state_text();
                let repo = t
                    .repo
                    .as_ref()
                    .and_then(|r| r.strip_prefix(&self.root).ok())
                    .unwrap_or(&t.folder)
                    .display()
                    .to_string();
                let age = now.saturating_sub(t.started);
                let age = match age {
                    0..3600 => format!("{}m", age / 60),
                    3600..86400 => format!("{}h", age / 3600),
                    _ => format!("{}d", age / 86400),
                };
                let mut line = format!(
                    "{} | {} | {state} | {} | {age}",
                    t.label,
                    if repo.is_empty() { "." } else { &repo },
                    t.model
                );
                if let Some(s) = &t.summary {
                    line += &format!(" | {s}");
                }
                if let Some(q) = &t.question {
                    line += &format!(" | asks: {q}");
                }
                line
            })
            .collect();
        if rows.is_empty() {
            "No tasks.".into()
        } else {
            rows.join("\n")
        }
    }

    /// A page of the task's final message (its last reply), by lines.
    pub(super) fn read(&self, t: &Task, offset: usize, limit: usize) -> Result<String, String> {
        let session = t
            .session
            .as_deref()
            .ok_or_else(|| format!("{} has not started", t.label))?;
        let paths = Paths::new(&self.cfg.home);
        let cwd = t.cwd.clone().unwrap_or_else(|| self.root.clone());
        let file = novacode_core::session::find(&paths, &cwd, session).map_err(|e| e.to_string())?;
        // A live task holds its session lock; read without it (R17).
        let store = novacode_core::session::Store::peek(&file).map_err(|e| e.to_string())?;
        let text = store
            .projection()
            .iter()
            .rev()
            .find_map(|m| match m {
                novacode_types::Message::Assistant(a) if !a.text().trim().is_empty() => Some(a.text()),
                _ => None,
            })
            .ok_or_else(|| format!("{} has no final message yet", t.label))?;
        let lines: Vec<&str> = text.lines().collect();
        let end = (offset + limit.max(1)).min(lines.len());
        let mut out = lines.get(offset..end).unwrap_or_default().join("\n");
        if end < lines.len() {
            out += &format!("\n[{} more lines: read with offset {end}]", lines.len() - end);
        }
        Ok(out)
    }
}

/// The review task of row `id` on the board as it is.
fn reviewer(b: &Board, id: &str) -> Option<String> {
    b.task(id).and_then(|t| t.reviewer.clone())
}

/// The worktree of `main`'s repo that has `branch` checked out.
fn worktree_of(main: &Path, branch: &str) -> Option<PathBuf> {
    let want = format!("branch refs/heads/{branch}");
    git(main, &["worktree", "list", "--porcelain"])
        .ok()?
        .split("\n\n")
        .find(|e| e.lines().any(|l| l == want))?
        .lines()
        .find_map(|l| l.strip_prefix("worktree "))
        .map(PathBuf::from)
}

/// Uncommitted changes to tracked files (untracked files, such as `.worktrees/`, don't count).
fn changes(dir: &Path) -> bool {
    git(dir, &["status", "--porcelain", "--untracked-files=no"]).is_ok_and(|s| !s.is_empty())
}

/// What an unfinished merge left in the task's main checkout or worktree: a merge or
/// rebase in progress, or uncommitted changes in the main checkout. A local merge starts
/// only in a clean main checkout, so any change there is the merge's.
pub(super) fn half_done(t: &Task) -> Option<String> {
    let main = t.repo.as_ref()?;
    let in_progress = |dir: &Path| -> Option<&'static str> {
        let gd = PathBuf::from(git(dir, &["rev-parse", "--path-format=absolute", "--git-dir"]).ok()?);
        if gd.join("MERGE_HEAD").exists() {
            Some("a merge in progress")
        } else if gd.join("rebase-merge").exists() || gd.join("rebase-apply").exists() {
            Some("a rebase in progress")
        } else {
            None
        }
    };
    if let Some(what) = in_progress(main) {
        return Some(format!("the main checkout {} has {what}", main.display()));
    }
    if changes(main) {
        return Some(format!("the main checkout {} has uncommitted changes", main.display()));
    }
    let wt = t.worktree.as_ref().filter(|w| w.exists())?;
    in_progress(wt).map(|what| format!("its worktree {} has {what}", wt.display()))
}

/// The `done` facts of a write task that never reported, from git: its branch head and the
/// files changed since its base; no checks. Refused when nothing is committed beyond the base.
fn commits_done(t: &Task) -> Result<super::board::Done, String> {
    let nothing = || format!("{}: nothing committed to review; send it on or close it", t.label);
    let (Some(main), Some(branch), Some(base)) = (&t.repo, &t.branch, &t.base_commit) else {
        return Err(nothing());
    };
    let ahead = git(main, &["rev-list", "--count", &format!("{base}..{branch}")])?;
    if ahead.trim() == "0" {
        return Err(nothing());
    }
    let head = git(main, &["rev-parse", branch])?;
    let copies = t
        .worktree
        .as_deref()
        .map(novacode_tools::worktree::copies)
        .unwrap_or_default();
    let files = git(main, &["diff", "--name-only", &format!("{base}...{head}")])?
        .lines()
        .filter(|f| !f.is_empty() && !copies.iter().any(|c| f.starts_with(c.as_str())))
        .map(String::from)
        .collect();
    Ok(super::board::Done {
        commit: Some(head),
        files,
        from_commits: true,
        ..Default::default()
    })
}

/// The non-git write task already holding `t`'s folder.
fn folder_holder<'a>(b: &'a Board, t: &Task) -> Option<&'a Task> {
    if !t.write || t.repo.is_some() || t.kind != Kind::Work {
        return None;
    }
    b.tasks.iter().find(|o| {
        o.id != t.id
            && o.write
            && o.repo.is_none()
            && o.folder == t.folder
            && !matches!(o.state, State::Queued | State::Archived)
    })
}

/// The queued task to start next, if a slot is free.
fn pick(b: &Board, max_running: usize) -> Option<String> {
    let running = b.tasks.iter().filter(|t| t.state == State::Running).count();
    if running >= max_running {
        return None;
    }
    b.tasks
        .iter()
        .filter(|t| t.state == State::Queued)
        .filter(|t| !b.holds.contains_key(provider(&t.model)))
        .filter(|t| folder_holder(b, t).is_none())
        .min_by_key(|t| (t.kind != Kind::Review, t.started, number(&t.label)))
        .map(|t| t.id.clone())
}

/// Super's `task` tool.
pub struct TaskTool(pub Arc<Super>);

impl Tool for TaskTool {
    fn name(&self) -> &str {
        "task"
    }
    fn description(&self) -> &str {
        "Start and steer tasks: separate novacode sessions that do the user's work. Actions: spawn {request, folder, slug, write, tier? | model?, type?, criteria[], scope?, skills?[], goal?}; info {folder} (changes nothing: whether the folder is in git, the branch a write task's worktree would start from, and trust; show it with the criteria before a non-trivial task starts); send {task, message, question?}; stop {task} (a running, queued or reviewing task; a blocked one is not running: close or send it); close {task} and merge {task, pr?} (each asks the user on a card; merge works on a ready task, or retries on a task blocked by a failed merge whose approval still covers its commit); accept {task} (a read task in reviewing, or blocked by code, e.g. it answered without a report; a write task blocked by used-up review rounds, after the user's OK on a card); review {task, note?} (a ready task again without a round; a write task blocked by code because it kept ending without a report: code takes its commit and changed files from its branch, refused when nothing is committed, and the review uses a round); list {all?}; read {task, offset?, limit?} (pages the task's final message)."
    }
    fn schema(&self) -> Value {
        let s = |d: &str| json!({ "type": "string", "description": d });
        json!({
            "type": "object",
            "properties": {
                "action": { "type": "string", "enum": ["spawn", "info", "send", "stop", "close", "accept", "merge", "review", "list", "read"] },
                "task": s("The task's label (T3-fix-parser) or T<n>."),
                "request": s("spawn: the user's words, as given."),
                "folder": s("spawn and info: the project folder, inside your root (relative to it or absolute)."),
                "slug": s("spawn: 1-4 words of a-z0-9 joined by -, at most 30 characters; the label becomes T<n>-<slug>."),
                "write": { "type": "boolean", "description": "spawn: the task changes files (it gets its own worktree in git)." },
                "tier": s("spawn: quick, standard, deep or review (default standard)."),
                "model": s("spawn: a model the user named (provider/model), instead of a tier."),
                "type": { "type": "string", "enum": ["bugfix", "change", "question"], "description": "spawn: question needs write false; bugfix and change need write true. Default change." },
                "criteria": { "type": "array", "items": { "type": "string" }, "description": "spawn: 1-5 checkable acceptance criteria." },
                "scope": s("spawn: the areas the change should touch."),
                "skills": { "type": "array", "items": { "type": "string" }, "description": "spawn: skills the task should use." },
                "goal": { "type": "boolean", "description": "spawn: open-ended work; the task keeps going until the criteria hold." },
                "message": s("send: the message."),
                "question": { "type": "boolean", "description": "send: only ask for information (raw output, proof): the task's state and review stay, it answers in text without a report, and its reply wakes you. Leave false for work: a plain send sets the task running and stops its review." },
                "pr": { "type": "boolean", "description": "merge: open a pull request instead of merging locally; only when the user asked for a pull request." },
                "note": s("review: what the review should look at."),
                "all": { "type": "boolean", "description": "list: archived tasks too." },
                "offset": { "type": "integer", "description": "read: first line (0-based)." },
                "limit": { "type": "integer", "description": "read: lines (default 200)." }
            },
            "required": ["action"]
        })
    }
    fn sequential(&self) -> bool {
        true
    }
    fn large_output(&self) -> bool {
        false
    }
    fn execute<'a>(&'a self, args: Value, _ctx: &'a ToolCtx) -> BoxFuture<'a, ToolOutput> {
        Box::pin(async move {
            match self.0.run(args).await {
                Ok(text) => ToolOutput::text(text),
                Err(e) => ToolOutput::error(e),
            }
        })
    }
}

/// How long a started task has to print its session id.
const START_WAIT: std::time::Duration = std::time::Duration::from_secs(60);

/// The session id line a started task prints first. A child that prints nothing within
/// `wait` has its process group killed: a hung start must not hang super's turn or tick.
fn session_id(mut child: std::process::Child, wait: std::time::Duration) -> Result<String, String> {
    use std::io::BufRead;
    let out = child.stdout.take();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut id = String::new();
        if let Some(out) = out {
            let _ = std::io::BufReader::new(out).read_line(&mut id);
        }
        let _ = tx.send(id);
    });
    let Ok(id) = rx.recv_timeout(wait) else {
        novacode_tools::process::kill_group_now(child.id());
        let _ = child.wait();
        return Err(format!(
            "the task's session didn't start within {}s (its process was stopped); see ~/.novacode/logs",
            wait.as_secs()
        ));
    };
    std::thread::spawn(move || child.wait());
    let id = id.trim().to_string();
    if id.is_empty() {
        Err("the task's session didn't start; see ~/.novacode/logs".to_string())
    } else {
        Ok(id)
    }
}

/// The real launcher: detached `novacode --bg` processes and their sockets.
pub struct Procs {
    /// The live path of this executable (after an update, not `… (deleted)`).
    pub exe: PathBuf,
    pub paths: Paths,
}

impl Launcher for Procs {
    fn start(&self, s: Start) -> BoxFuture<'_, Result<String, String>> {
        let exe = self.exe.clone();
        Box::pin(async move {
            tokio::task::spawn_blocking(move || {
                // A missing cwd fails spawn with ENOENT, which reads as a missing binary.
                if !s.cwd.is_dir() {
                    return Err(format!("the task folder {} no longer exists", s.cwd.display()));
                }
                let mut c = std::process::Command::new(&exe);
                c.arg("--bg").args(["-m", &s.model]);
                if s.trust {
                    c.arg("--trust");
                }
                if let Some(r) = &s.resume {
                    c.args(["-r", r]);
                }
                // Not spawn_bg: its NOVACODE_BG_CHILD would reach everything the task starts.
                c.current_dir(&s.cwd)
                    .env_remove("NOVACODE_BG_CHILD")
                    .envs(s.env.iter().map(|(k, v)| (k, v)))
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::null());
                novacode_base::platform::own_group(&mut c);
                let child = c.spawn().map_err(|e| format!("{}: {e}", exe.display()))?;
                session_id(child, START_WAIT)
            })
            .await
            .map_err(|e| e.to_string())?
        })
    }

    fn send_as<'a>(&'a self, session: &'a str, rid: &'a str, cmd: ClientCmd) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
            let f = crate::serve::find_live(&self.paths, session).map_err(|e| e.to_string())?;
            let shutdown = matches!(cmd, ClientCmd::Shutdown);
            let talk = async {
                let s = crate::serve::connect(&f.socket).await.map_err(|e| e.to_string())?;
                let (r, mut w) = tokio::io::split(s);
                let req = Request {
                    id: Some(rid.into()),
                    cmd,
                };
                w.write_all(line(&req).as_bytes()).await.map_err(|e| e.to_string())?;
                w.flush().await.map_err(|e| e.to_string())?;
                let mut lines = tokio::io::BufReader::new(r).lines();
                while let Ok(Some(l)) = lines.next_line().await {
                    match serde_json::from_str::<ServerMsg>(&l) {
                        Ok(ServerMsg::Response {
                            id: Some(id),
                            ok,
                            error,
                            ..
                        }) if id == rid => {
                            return if ok { Ok(()) } else { Err(error.unwrap_or_default()) };
                        }
                        Ok(ServerMsg::Bye { .. }) if shutdown => return Ok(()),
                        _ => {}
                    }
                }
                if shutdown {
                    Ok(())
                } else {
                    Err("the task closed the connection".to_string())
                }
            };
            tokio::time::timeout(std::time::Duration::from_secs(15), talk)
                .await
                .map_err(|_| format!("session {session} didn't answer"))?
        })
    }

    fn alive(&self, session: &str) -> bool {
        crate::serve::find_live(&self.paths, session).is_ok()
    }

    fn link<'a>(&'a self, session: &'a str) -> BoxFuture<'a, Result<super::link::Conn, String>> {
        Box::pin(async move {
            let f = crate::serve::find_live(&self.paths, session).map_err(|e| e.to_string())?;
            let s = crate::serve::connect(&f.socket).await.map_err(|e| e.to_string())?;
            let (r, w) = tokio::io::split(s);
            Ok(super::link::Conn {
                read: Box::new(r),
                write: Box::new(w),
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use novacode_types::Reply;
    use std::collections::{HashSet, VecDeque};

    /// What the fakes saw, in order: `start <cwd>`, `send <session> <command>`, `ask <header>`.
    type Log = Arc<Mutex<Vec<String>>>;

    #[derive(Default)]
    struct FakeLauncher {
        log: Log,
        starts: Mutex<Vec<Start>>,
        sent: Mutex<Vec<(String, ClientCmd)>>,
        /// The request id of each sent command.
        ids: Mutex<Vec<String>>,
        alive: Mutex<HashSet<String>>,
        /// Every start fails (a model without a login, …).
        fail_start: Mutex<bool>,
    }

    impl Launcher for FakeLauncher {
        fn start(&self, s: Start) -> BoxFuture<'_, Result<String, String>> {
            Box::pin(async move {
                if *self.fail_start.lock().unwrap() {
                    return Err("no login".to_string());
                }
                let mut starts = self.starts.lock().unwrap();
                let id = s.resume.clone().unwrap_or_else(|| format!("s{}", starts.len() + 1));
                self.log.lock().unwrap().push(format!("start {}", s.cwd.display()));
                starts.push(s);
                self.alive.lock().unwrap().insert(id.clone());
                Ok(id)
            })
        }
        fn send_as<'a>(&'a self, session: &'a str, id: &'a str, cmd: ClientCmd) -> BoxFuture<'a, Result<(), String>> {
            Box::pin(async move {
                self.ids.lock().unwrap().push(id.to_string());
                self.log.lock().unwrap().push(format!("send {session} {}", cmd.name()));
                if matches!(cmd, ClientCmd::Shutdown) {
                    self.alive.lock().unwrap().remove(session);
                }
                self.sent.lock().unwrap().push((session.to_string(), cmd));
                Ok(())
            })
        }
        fn alive(&self, session: &str) -> bool {
            self.alive.lock().unwrap().contains(session)
        }
    }

    impl FakeLauncher {
        fn prompts(&self, session: &str) -> Vec<String> {
            self.sent
                .lock()
                .unwrap()
                .iter()
                .filter(|(s, _)| s == session)
                .filter_map(|(_, c)| match c {
                    ClientCmd::Prompt { text, raw: true } => Some(text.clone()),
                    _ => None,
                })
                .collect()
        }
    }

    /// Answers cards in order: `Some(label)` picks that choice, `None` dismisses.
    struct FakeAsker {
        log: Log,
        answers: Mutex<VecDeque<Option<&'static str>>>,
        asked: Mutex<Vec<Question>>,
        /// Runs once while the next card is open (the board changing meanwhile).
        hook: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    }

    impl Asker for FakeAsker {
        fn ask(&self, questions: Vec<Question>) -> BoxFuture<'_, Option<Vec<Reply>>> {
            Box::pin(async move {
                let hook = self.hook.lock().unwrap().take();
                if let Some(h) = hook {
                    h();
                }
                let q = questions[0].clone();
                self.log
                    .lock()
                    .unwrap()
                    .push(format!("ask {}", q.header.clone().unwrap_or_default()));
                self.asked.lock().unwrap().push(q.clone());
                let a = self.answers.lock().unwrap().pop_front().flatten()?;
                Some(vec![Reply {
                    question: q.question,
                    chosen: vec![a.to_string()],
                    text: None,
                    preview: None,
                }])
            })
        }
    }

    struct Fx {
        _tmp: tempfile::TempDir,
        root: PathBuf,
        home: PathBuf,
        sup: Arc<Super>,
        launcher: Arc<FakeLauncher>,
        asker: Arc<FakeAsker>,
        log: Log,
    }

    fn fixture(max_running: usize, answers: &[Option<&'static str>]) -> Fx {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().canonicalize().unwrap();
        let (root, home) = (base.join("root"), base.join("home"));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&home).unwrap();
        let log: Log = Arc::default();
        let launcher = Arc::new(FakeLauncher {
            log: log.clone(),
            ..FakeLauncher::default()
        });
        let asker = Arc::new(FakeAsker {
            log: log.clone(),
            answers: Mutex::new(answers.iter().copied().collect()),
            asked: Mutex::default(),
            hook: Mutex::default(),
        });
        let tiers = ["quick", "standard", "deep", "review"]
            .iter()
            .map(|n| {
                (
                    n.to_string(),
                    Tier {
                        model: format!("faux/{n}"),
                        note: String::new(),
                    },
                )
            })
            .collect();
        let cfg = Config {
            max_running,
            tiers,
            worktree_copy: vec!["AGENTS.md".into(), ".codegraph".into()],
            home: home.clone(),
            allow: Arc::new(|m: &str| match m.starts_with("denied/") {
                true => Err(format!("{m} is denied by models.deny")),
                false => Ok(()),
            }),
        };
        let sup =
            Arc::new(Super::new(&root, home.join("super/x"), cfg, Some(asker.clone()), launcher.clone()).unwrap());
        Fx {
            _tmp: tmp,
            root,
            home,
            sup,
            launcher,
            asker,
            log,
        }
    }

    fn sh(dir: &Path, args: &[&str]) {
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

    /// `<root>/<name>`: a repo on branch `main` with one commit and a `sub/` folder.
    fn repo(root: &Path, name: &str) -> PathBuf {
        let r = root.join(name);
        std::fs::create_dir_all(r.join("sub")).unwrap();
        std::fs::write(r.join("sub/f"), "x").unwrap();
        sh(&r, &["init", "-q", "-b", "main"]);
        sh(&r, &["add", "."]);
        sh(&r, &["commit", "-qm", "init"]);
        r
    }

    fn spawn_args(folder: &str, slug: &str, write: bool) -> Value {
        json!({
            "action": "spawn", "request": format!("do {slug}"), "folder": folder, "slug": slug,
            "write": write, "type": if write { "change" } else { "question" }, "criteria": ["it works"]
        })
    }

    async fn run(fx: &Fx, args: Value) -> Result<String, String> {
        fx.sup.run(args).await
    }

    fn state(fx: &Fx, id: &str) -> State {
        fx.sup.row(id).unwrap().state
    }

    fn set_state(fx: &Fx, id: &str, s: State) {
        fx.sup
            .edit(|b| b.tasks.iter_mut().find(|t| t.id == id).unwrap().state = s)
            .unwrap();
    }

    #[tokio::test]
    async fn a_missing_task_folder_is_named_not_reported_as_a_missing_binary() {
        let tmp = tempfile::tempdir().unwrap();
        let procs = Procs {
            exe: PathBuf::from("/bin/true"),
            paths: Paths::new(tmp.path()),
        };
        let gone = tmp.path().join("gone");
        let e = procs
            .start(Start {
                cwd: gone.clone(),
                model: "m".into(),
                trust: false,
                resume: None,
                env: vec![],
            })
            .await
            .unwrap_err();
        assert_eq!(e, format!("the task folder {} no longer exists", gone.display()));
    }

    #[tokio::test]
    async fn bad_slugs_and_type_write_mismatches_are_refused() {
        let fx = fixture(5, &[]);
        std::fs::create_dir(fx.root.join("p")).unwrap();
        for bad in ["Fix", "a-b-c-d-e", "", "fix_it", "a--b", &"x".repeat(31)] {
            let e = run(&fx, spawn_args("p", bad, false)).await.unwrap_err();
            assert!(e.contains("bad slug"), "{bad}: {e}");
        }
        check_slug("fix-the-parser-2").unwrap();
        for (ty, write) in [("question", true), ("change", false), ("bugfix", false)] {
            let mut a = spawn_args("p", "x", write);
            a["type"] = json!(ty);
            let e = run(&fx, a).await.unwrap_err();
            assert!(e.contains(&format!("`type: {ty}` needs")), "{e}");
        }
        let mut a = spawn_args("p", "x", false);
        a["model"] = json!("denied/m");
        assert!(run(&fx, a).await.unwrap_err().contains("denied"));
        assert!(fx.sup.board.lock().unwrap().tasks.is_empty());
        assert!(fx.launcher.starts.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn folders_outside_the_root_are_refused_with_symlinks_resolved() {
        let fx = fixture(5, &[]);
        let outside = fx.home.join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&outside, fx.root.join("link")).unwrap();
            let e = run(&fx, spawn_args("link", "x", false)).await.unwrap_err();
            assert!(e.contains("outside the super root"), "{e}");
        }
        let e = run(&fx, spawn_args(outside.to_str().unwrap(), "x", false))
            .await
            .unwrap_err();
        assert!(e.contains("outside the super root"), "{e}");
        let e = run(&fx, spawn_args("../home", "x", false)).await.unwrap_err();
        assert!(e.contains("outside the super root"), "{e}");
        std::fs::create_dir(fx.root.join("in")).unwrap();
        run(&fx, spawn_args("in", "x", false)).await.unwrap();
    }

    #[tokio::test]
    async fn a_write_task_starts_in_its_own_worktree_with_env_and_a_raw_brief() {
        let fx = fixture(5, &[]);
        let app = repo(&fx.root, "app");
        std::fs::write(app.join("AGENTS.md"), "untracked rule").unwrap();
        let out = run(&fx, spawn_args("app/sub", "fix-it", true)).await.unwrap();
        assert!(out.contains("Started T1-fix-it"), "{out}");
        let t = fx.sup.row("T1-fix-it").unwrap();
        let wt = app.join(".worktrees/T1-fix-it");
        assert_eq!(t.state, State::Running);
        assert_eq!(t.worktree.as_deref(), Some(wt.as_path()));
        assert_eq!(t.branch.as_deref(), Some("worktree-T1-fix-it"));
        assert_eq!(t.base_branch.as_deref(), Some("main"));
        assert_eq!(t.base_commit, git(&app, &["rev-parse", "HEAD"]).ok());
        assert_eq!(t.repo.as_deref(), Some(app.as_path()));
        assert_eq!(t.session.as_deref(), Some("s1"));
        assert!(wt.join("AGENTS.md").exists(), "worktree.copy applied");
        let s = fx.launcher.starts.lock().unwrap()[0].clone();
        assert_eq!(s.cwd, wt.join("sub"), "same subfolder inside the worktree");
        assert_eq!(s.resume, None);
        let env: std::collections::HashMap<_, _> = s.env.into_iter().collect();
        assert_eq!(env["NOVACODE_TASK"], "T1-fix-it");
        assert_eq!(env["NOVACODE_TASK_TOKEN"], t.token);
        assert!(t.token.len() >= 32);
        assert_eq!(env["NOVACODE_SUPER_DIR"], fx.sup.dir.to_string_lossy());
        assert_eq!(env["NOVACODE_EDIT_RULE"], format!("worktree:{}", wt.display()));
        let brief = fx.launcher.prompts("s1");
        assert_eq!(brief.len(), 1);
        assert!(
            brief[0].contains("do fix-it") && brief[0].contains("1. it works"),
            "{}",
            brief[0]
        );
        // Board saved; no project config, so no trust card.
        assert_eq!(
            Board::load(&fx.sup.dir)
                .unwrap()
                .task("T1-fix-it")
                .unwrap()
                .session
                .as_deref(),
            Some("s1")
        );
        assert!(fx.asker.asked.lock().unwrap().is_empty());
        // Labels count up and are never reused; a read task runs in place.
        std::fs::create_dir(fx.root.join("docs")).unwrap();
        run(&fx, spawn_args("docs", "why", false)).await.unwrap();
        let s = fx.launcher.starts.lock().unwrap()[1].clone();
        assert_eq!(s.cwd, fx.root.join("docs"));
        assert!(fx.sup.row("T2-why").unwrap().worktree.is_none());
    }

    #[tokio::test]
    async fn an_untrusted_project_starts_only_after_the_trust_card() {
        let fx = fixture(5, &[Some("Trust for this task only"), Some("Don't trust")]);
        let app = repo(&fx.root, "app");
        std::fs::write(app.join(".mcp.json"), r#"{"mcpServers": {}}"#).unwrap();
        run(&fx, spawn_args("app", "one", true)).await.unwrap();
        let log = fx.log.lock().unwrap().clone();
        assert_eq!(log[0], "ask T1-one: trust", "{log:?}");
        assert!(log[1].starts_with("start "), "{log:?}");
        let q = fx.asker.asked.lock().unwrap()[0].question.clone();
        assert!(q.contains("branches from main"), "{q}");
        assert!(fx.sup.row("T1-one").unwrap().trusted);
        assert!(fx.launcher.starts.lock().unwrap()[0].trust);
        // "Don't trust": it starts without the project config, and the answer is stored.
        run(&fx, spawn_args("app", "two", true)).await.unwrap();
        assert!(!fx.launcher.starts.lock().unwrap()[1].trust);
        let store = novacode_core::trust::TrustStore::new(fx.home.join("trust.json"));
        assert_eq!(store.decision(&app), Some(false));
    }

    #[tokio::test]
    async fn trusting_the_folder_on_the_card_stores_it() {
        let fx = fixture(5, &[Some("Trust this folder")]);
        let app = repo(&fx.root, "app");
        std::fs::write(app.join(".mcp.json"), r#"{"mcpServers": {}}"#).unwrap();
        run(&fx, spawn_args("app", "one", true)).await.unwrap();
        let store = novacode_core::trust::TrustStore::new(fx.home.join("trust.json"));
        assert_eq!(store.state(&app), novacode_core::trust::State::Trusted);
        // Trusted now: the next task asks nothing.
        run(&fx, spawn_args("app", "two", true)).await.unwrap();
        assert_eq!(fx.asker.asked.lock().unwrap().len(), 1);
        assert!(fx.launcher.starts.lock().unwrap()[1].trust);
    }

    #[tokio::test]
    async fn the_running_limit_queues_and_stop_frees_a_slot() {
        let fx = fixture(5, &[]);
        std::fs::create_dir(fx.root.join("p")).unwrap();
        for i in 1..=7 {
            run(&fx, spawn_args("p", &format!("t{i}"), false)).await.unwrap();
        }
        assert_eq!(fx.launcher.starts.lock().unwrap().len(), 5);
        assert_eq!(state(&fx, "T6-t6"), State::Queued);
        // Stop on a queued task takes it out of the queue; nothing is sent.
        let out = run(&fx, json!({"action": "stop", "task": "T6"})).await.unwrap();
        assert!(out.contains("out of the queue"), "{out}");
        assert_eq!(state(&fx, "T6-t6"), State::Stopped);
        // A send to a queued task joins its brief.
        run(&fx, json!({"action": "send", "task": "T7-t7", "message": "also X"}))
            .await
            .unwrap();
        // Stop on a running one: Abort that drops queued input, and the queue moves on.
        run(&fx, json!({"action": "stop", "task": "T1-t1"})).await.unwrap();
        let sent = fx.launcher.sent.lock().unwrap().clone();
        assert!(
            sent.iter()
                .any(|(s, c)| s == "s1" && matches!(c, ClientCmd::Abort { drop_queued: true }))
        );
        assert_eq!(state(&fx, "T1-t1"), State::Stopped);
        assert_eq!(state(&fx, "T7-t7"), State::Running);
        let brief = fx.launcher.prompts("s6");
        assert!(brief[0].contains("also X"), "{brief:?}");
        // A stopped task can't be stopped again: the pair is not in the table.
        let e = run(&fx, json!({"action": "stop", "task": "T1-t1"})).await.unwrap_err();
        assert!(e.contains("not possible"), "{e}");
    }

    #[tokio::test]
    async fn reviews_start_first_and_held_providers_wait() {
        let fx = fixture(1, &[]);
        std::fs::create_dir(fx.root.join("p")).unwrap();
        run(&fx, spawn_args("p", "busy", false)).await.unwrap();
        run(&fx, spawn_args("p", "work", false)).await.unwrap();
        let mut a = spawn_args("p", "held", false);
        a["model"] = json!("held/m");
        run(&fx, a).await.unwrap();
        fx.sup
            .edit(|b| {
                b.holds.insert(
                    "held".into(),
                    super::super::board::Limit {
                        kind: super::super::board::LimitKind::Quota,
                        provider: "held".into(),
                        reset_at: None,
                    },
                );
                b.tasks.push(Task {
                    id: "T9-review-x".into(),
                    label: "T9-review-x".into(),
                    kind: Kind::Review,
                    model: "faux/review".into(),
                    state: State::Queued,
                    started: u64::MAX,
                    ..Task::default()
                });
            })
            .unwrap();
        let why = fx.sup.why_queued(&fx.sup.row("T3-held").unwrap());
        assert!(why.contains("on hold"), "{why}");
        run(&fx, json!({"action": "stop", "task": "T1-busy"})).await.unwrap();
        assert_eq!(state(&fx, "T9-review-x"), State::Running, "the review goes first");
        assert_eq!(state(&fx, "T2-work"), State::Queued);
        set_state(&fx, "T9-review-x", State::Archived);
        fx.sup.pump().await;
        assert_eq!(state(&fx, "T2-work"), State::Running);
        set_state(&fx, "T2-work", State::Stopped);
        fx.sup.pump().await;
        assert_eq!(state(&fx, "T3-held"), State::Queued, "its provider is on hold");
    }

    #[tokio::test]
    async fn a_queued_write_task_names_the_branch_it_will_start_from() {
        let fx = fixture(1, &[]);
        repo(&fx.root, "app");
        run(&fx, spawn_args("app", "a", true)).await.unwrap();
        let out = run(&fx, spawn_args("app", "b", true)).await.unwrap();
        assert_eq!(state(&fx, "T2-b"), State::Queued);
        assert!(out.contains("branches from main"), "{out}");
    }

    #[tokio::test]
    async fn a_second_non_git_write_task_in_one_folder_queues() {
        let fx = fixture(5, &[]);
        std::fs::create_dir(fx.root.join("plain")).unwrap();
        run(&fx, spawn_args("plain", "one", true)).await.unwrap();
        let out = run(&fx, spawn_args("plain", "two", true)).await.unwrap();
        assert!(out.contains("queued") && out.contains("T1-one"), "{out}");
        let s = fx.launcher.starts.lock().unwrap()[0].clone();
        let rule = s.env.iter().find(|(k, _)| k == "NOVACODE_EDIT_RULE").unwrap();
        assert_eq!(rule.1, format!("worktree:{}", fx.root.join("plain").display()));
        let out = run(&fx, json!({"action": "close", "task": "T1-one"})).await.unwrap();
        assert!(out.contains("declined"), "a dismissed card: {out}");
        set_state(&fx, "T1-one", State::Archived);
        fx.sup.pump().await;
        assert_eq!(state(&fx, "T2-two"), State::Running);
    }

    #[tokio::test]
    async fn send_continues_every_resumable_state_and_resumes_a_dead_session() {
        let fx = fixture(5, &[]);
        std::fs::create_dir(fx.root.join("p")).unwrap();
        run(&fx, spawn_args("p", "t", false)).await.unwrap();
        use State::*;
        for (i, s) in [Blocked, Failed, Stopped, Reviewing, Ready, Interrupted]
            .into_iter()
            .enumerate()
        {
            set_state(&fx, "T1-t", s);
            let dead = i % 2 == 1;
            if dead {
                fx.launcher.alive.lock().unwrap().remove("s1");
            }
            let out = run(
                &fx,
                json!({"action": "send", "task": "T1-t", "message": format!("m{i}")}),
            )
            .await
            .unwrap();
            assert!(out.contains("running"), "{s:?}: {out}");
            assert_eq!(state(&fx, "T1-t"), Running, "{s:?}");
            assert_eq!(fx.launcher.prompts("s1").last().unwrap(), &format!("m{i}"));
        }
        let starts = fx.launcher.starts.lock().unwrap().clone();
        assert_eq!(starts.len(), 4, "three resumes after the first start");
        assert!(starts[1..].iter().all(|s| s.resume.as_deref() == Some("s1")));
        // While running the message is just delivered; archived tasks refuse it.
        run(&fx, json!({"action": "send", "task": "T1-t", "message": "more"}))
            .await
            .unwrap();
        set_state(&fx, "T1-t", Archived);
        let e = run(&fx, json!({"action": "send", "task": "T1-t", "message": "x"}))
            .await
            .unwrap_err();
        assert!(e.contains("not possible"), "{e}");
    }

    #[tokio::test]
    async fn a_question_send_keeps_the_state_and_is_marked_as_a_question() {
        let fx = fixture(5, &[]);
        std::fs::create_dir(fx.root.join("p")).unwrap();
        run(&fx, spawn_args("p", "t", false)).await.unwrap();
        // The brief's run ended (questions and work never share a run).
        fx.sup.ran("s1", 1).await;
        use State::*;
        for s in [Ready, Running, Blocked] {
            set_state(&fx, "T1-t", s);
            let ask = json!({"action": "send", "task": "T1-t", "message": "paste check #9", "question": true});
            let out = run(&fx, ask).await.unwrap();
            assert!(out.contains("Asked T1-t"), "{out}");
            assert_eq!(state(&fx, "T1-t"), s, "{s:?}");
            let p = fx.launcher.prompts("s1").pop().unwrap();
            assert!(
                p.starts_with(super::super::link::QUESTION_PREFIX) && p.ends_with("paste check #9"),
                "{p}"
            );
            assert_eq!(
                fx.launcher.ids.lock().unwrap().last().unwrap(),
                super::super::link::SUPER_QUESTION_ID
            );
        }
        assert!(
            fx.sup.row("T1-t").unwrap().follow_ups.is_empty(),
            "a question is no follow-up"
        );
        // A plain send is unchanged once the questions' runs ended.
        fx.sup.ran("s1", 3).await;
        set_state(&fx, "T1-t", Ready);
        run(&fx, json!({"action": "send", "task": "T1-t", "message": "work"}))
            .await
            .unwrap();
        assert_eq!(state(&fx, "T1-t"), Running);
        assert_eq!(fx.launcher.prompts("s1").last().unwrap(), "work");
        assert_eq!(
            fx.launcher.ids.lock().unwrap().last().unwrap(),
            super::super::link::SUPER_ID
        );
        // A queued task has no session to answer.
        set_state(&fx, "T1-t", Queued);
        let ask = json!({"action": "send", "task": "T1-t", "message": "q", "question": true});
        assert!(run(&fx, ask).await.is_err());
    }

    #[tokio::test]
    async fn a_question_and_a_work_prompt_are_never_queued_into_one_run() {
        // L10.9 R24: the second kind waits in code until the run that took the first one ends.
        let fx = fixture(5, &[]);
        std::fs::create_dir(fx.root.join("p")).unwrap();
        run(&fx, spawn_args("p", "t", false)).await.unwrap();
        let sent = || fx.launcher.prompts("s1");
        let ask = |m: &str| json!({"action": "send", "task": "T1-t", "message": m, "question": true});
        // The brief's run is going: a question waits for its end.
        let out = run(&fx, ask("which check?")).await.unwrap();
        assert!(out.contains("held"), "{out}");
        assert_eq!(sent().len(), 1);
        fx.sup.ran("s1", 1).await;
        assert!(sent().last().unwrap().ends_with("which check?"));
        assert_eq!(
            fx.launcher.ids.lock().unwrap().last().unwrap(),
            super::super::link::SUPER_QUESTION_ID
        );
        // The question's run is going: work waits for its end.
        let out = run(&fx, json!({"action": "send", "task": "T1-t", "message": "work"}))
            .await
            .unwrap();
        assert!(out.contains("held"), "{out}");
        assert_eq!(sent().len(), 2);
        fx.sup.ran("s1", 1).await;
        assert_eq!(sent().last().unwrap(), "work");
        assert_eq!(
            fx.launcher.ids.lock().unwrap().last().unwrap(),
            super::super::link::SUPER_ID
        );
        // A stop drops what waits.
        run(&fx, ask("later?")).await.unwrap();
        run(&fx, json!({"action": "stop", "task": "T1-t"})).await.unwrap();
        fx.sup.ran("s1", 1).await;
        assert_eq!(sent().len(), 3, "{:?}", sent());
    }

    #[tokio::test]
    async fn close_asks_on_a_card_and_keeps_an_unmerged_worktree() {
        let fx = fixture(5, &[None, Some("Close"), Some("Close")]);
        let app = repo(&fx.root, "app");
        run(&fx, spawn_args("app", "keep", true)).await.unwrap();
        let wt = app.join(".worktrees/T1-keep");
        std::fs::write(wt.join("new"), "y").unwrap();
        sh(&wt, &["add", "new"]);
        sh(&wt, &["commit", "-qm", "work"]);
        // Dismissed card: nothing happens.
        let out = run(&fx, json!({"action": "close", "task": "T1-keep"})).await.unwrap();
        assert!(out.contains("declined"), "{out}");
        assert_eq!(state(&fx, "T1-keep"), State::Running);
        assert!(fx.launcher.alive("s1"));
        let q = fx.asker.asked.lock().unwrap()[0].question.clone();
        assert!(q.contains("worktree-T1-keep is NOT merged into main"), "{q}");
        // Confirmed: archived, process ended, unmerged worktree kept.
        run(&fx, json!({"action": "close", "task": "T1-keep"})).await.unwrap();
        assert_eq!(state(&fx, "T1-keep"), State::Archived);
        assert!(!fx.launcher.alive("s1"));
        assert!(wt.exists());
        // A branch merged into its base: the worktree goes.
        run(&fx, spawn_args("app", "done", true)).await.unwrap();
        let wt2 = app.join(".worktrees/T2-done");
        let out = run(&fx, json!({"action": "close", "task": "T2-done"})).await.unwrap();
        let q = fx.asker.asked.lock().unwrap()[2].question.clone();
        assert!(q.contains("is merged into main"), "{q}");
        assert!(!wt2.exists(), "{out}");
    }

    #[tokio::test]
    async fn close_on_an_unmerged_branch_can_remove_the_worktree_and_keeps_the_branch() {
        let fx = fixture(5, &[Some("Close and remove worktree")]);
        let app = repo(&fx.root, "app");
        run(&fx, spawn_args("app", "drop", true)).await.unwrap();
        let wt = app.join(".worktrees/T1-drop");
        std::fs::write(wt.join("new"), "y").unwrap();
        sh(&wt, &["add", "new"]);
        sh(&wt, &["commit", "-qm", "work"]);
        let out = run(&fx, json!({"action": "close", "task": "T1-drop"})).await.unwrap();
        let q = fx.asker.asked.lock().unwrap()[0].clone();
        let labels: Vec<&str> = q.options.iter().map(|o| o.label.as_str()).collect();
        assert_eq!(labels, ["Close", "Close and remove worktree", "Cancel"], "{q:?}");
        assert_eq!(state(&fx, "T1-drop"), State::Archived);
        assert!(!wt.exists(), "{out}");
        let branch = fx.sup.row("T1-drop").unwrap().branch.unwrap();
        assert!(
            git(&app, &["rev-parse", "--verify", &branch]).is_ok(),
            "the branch stays: {out}"
        );
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn close_stops_a_daemon_left_in_the_worktree_and_removes_it() {
        // Nothing committed: the branch is merged, so a plain close removes the worktree.
        let fx = fixture(5, &[Some("Close")]);
        let app = repo(&fx.root, "app");
        std::fs::create_dir_all(app.join(".codegraph")).unwrap();
        std::fs::write(app.join(".codegraph/codegraph.db"), "db").unwrap();
        run(&fx, spawn_args("app", "cg", true)).await.unwrap();
        let wt = app.join(".worktrees/T1-cg");
        assert!(wt.join(".codegraph/codegraph.db").exists(), "copied");
        // A detached daemon that keeps writing in the copied folder, like codegraph's.
        let mut daemon = std::process::Command::new("sh")
            .args([
                "-c",
                "while :; do echo x > writer.pid; echo x > codegraph.db-wal; sleep 0.01; done",
            ])
            .current_dir(wt.join(".codegraph"))
            .spawn()
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(100));
        let out = run(&fx, json!({"action": "close", "task": "T1-cg"})).await;
        let gone = daemon.try_wait().unwrap().is_some();
        let _ = daemon.kill();
        let _ = daemon.wait();
        assert!(gone, "the daemon is stopped: {out:?}");
        assert!(!wt.exists(), "{out:?}");
    }

    #[tokio::test]
    async fn merge_asks_on_a_card_and_does_nothing_on_decline() {
        let fx = fixture(5, &[Some("Cancel"), Some("Merge")]);
        repo(&fx.root, "app");
        run(&fx, spawn_args("app", "m", true)).await.unwrap();
        let e = run(&fx, json!({"action": "merge", "task": "T1-m"})).await.unwrap_err();
        assert!(e.contains("not possible"), "not ready yet: {e}");
        assert!(fx.asker.asked.lock().unwrap().is_empty(), "refused before any card");
        set_state(&fx, "T1-m", State::Ready);
        let before = fx.launcher.prompts("s1").len();
        let out = run(&fx, json!({"action": "merge", "task": "T1-m"})).await.unwrap();
        assert!(out.contains("declined"), "{out}");
        assert_eq!(state(&fx, "T1-m"), State::Ready);
        assert_eq!(fx.launcher.prompts("s1").len(), before);
        run(&fx, json!({"action": "merge", "task": "T1-m"})).await.unwrap();
        assert_eq!(state(&fx, "T1-m"), State::Merging);
        assert!(fx.launcher.prompts("s1").last().unwrap().contains("merge: merged"));
        let q = fx.asker.asked.lock().unwrap()[1].question.clone();
        assert!(q.contains("Merge T1-m?") && q.contains("worktree-T1-m"), "{q}");
    }

    #[tokio::test]
    async fn accept_of_a_read_task_blocked_by_code_needs_no_card() {
        let fx = fixture(5, &[]);
        std::fs::create_dir(fx.root.join("p")).unwrap();
        run(&fx, spawn_args("p", "q", false)).await.unwrap();
        fx.sup
            .edit(|b| b.apply("T1-q", Event::TurnUnreported))
            .unwrap()
            .unwrap();
        run(&fx, json!({"action": "accept", "task": "T1-q"})).await.unwrap();
        assert_eq!(state(&fx, "T1-q"), State::Ready);
        assert!(fx.asker.asked.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn accept_review_list_read_and_review_rows() {
        let fx = fixture(5, &[]);
        std::fs::create_dir(fx.root.join("p")).unwrap();
        run(&fx, spawn_args("p", "q", false)).await.unwrap();
        set_state(&fx, "T1-q", State::Reviewing);
        run(&fx, json!({"action": "accept", "task": "T1-q"})).await.unwrap();
        assert_eq!(state(&fx, "T1-q"), State::Ready);
        // A doubted approval: back to reviewing, no round used.
        run(&fx, json!({"action": "review", "task": "T1-q"})).await.unwrap();
        let t = fx.sup.row("T1-q").unwrap();
        assert_eq!((t.state, t.review_rounds), (State::Reviewing, 0));
        fx.sup
            .edit(|b| {
                b.tasks.push(Task {
                    id: "T2-review-q".into(),
                    label: "T2-review-q".into(),
                    kind: Kind::Review,
                    reviewer_of: Some("T1-q".into()),
                    state: State::Running,
                    model: "faux/review".into(),
                    ..Task::default()
                })
            })
            .unwrap();
        for action in ["send", "stop", "close", "merge", "accept", "review"] {
            let e = run(&fx, json!({"action": action, "task": "T2-review-q", "message": "x"}))
                .await
                .unwrap_err();
            assert!(e.contains("review task"), "{action}: {e}");
        }
        let list = run(&fx, json!({"action": "list"})).await.unwrap();
        assert!(list.contains("T1-q | p | reviewing | faux/standard"), "{list}");
        assert!(list.contains("T2-review-q"), "{list}");
        let e = run(&fx, json!({"action": "read", "task": "T2-review-q"}))
            .await
            .unwrap_err();
        assert!(!e.contains("review task"), "read works on a review row: {e}");
        let e = run(&fx, json!({"action": "fly", "task": "T1-q"})).await.unwrap_err();
        assert!(e.contains("unknown action"), "{e}");
    }

    #[tokio::test]
    async fn read_works_on_a_live_task_whose_session_is_locked() {
        use novacode_core::session::{EntryKind, Store};
        let fx = fixture(5, &[]);
        let mut s = Store::create(&Paths::new(&fx.home), &fx.root, false);
        s.append(EntryKind::Message {
            message: novacode_types::Message::user("q"),
        })
        .unwrap();
        s.append(EntryKind::Message {
            message: novacode_types::Message::Assistant(novacode_types::AssistantMessage {
                content: vec![novacode_types::Block::text("the answer")],
                stop: novacode_types::StopReason::Stop,
                usage: Default::default(),
                provider: "faux".into(),
                model: "m".into(),
                response_id: None,
                error_message: None,
                thinking_level: None,
            }),
        })
        .unwrap();
        let file = s.path().unwrap().to_path_buf();
        drop(s);
        // The live task's own process holds the lock (R17).
        let _live = Store::open(&file).unwrap();
        fx.sup
            .edit(|b| {
                b.tasks.push(Task {
                    id: "T1-q".into(),
                    label: "T1-q".into(),
                    state: State::Running,
                    session: Some(file.display().to_string()),
                    ..Task::default()
                })
            })
            .unwrap();
        let out = run(&fx, json!({"action": "read", "task": "T1-q"})).await.unwrap();
        assert_eq!(out, "the answer");
    }

    fn lock_holders(fx: &Fx) -> Vec<String> {
        fx.sup.board.lock().unwrap().merge_locks.values().cloned().collect()
    }

    #[tokio::test]
    async fn a_main_checkout_on_another_branch_fails_the_merge_without_touching_it() {
        let fx = fixture(5, &[Some("Merge")]);
        let app = repo(&fx.root, "app");
        run(&fx, spawn_args("app", "m", true)).await.unwrap();
        set_state(&fx, "T1-m", State::Ready);
        sh(&app, &["checkout", "-qb", "other"]);
        let before = fx.launcher.prompts("s1").len();
        let out = run(&fx, json!({"action": "merge", "task": "T1-m"})).await.unwrap();
        assert!(
            out.contains("merge: failed") && out.contains("main checkout is on other, not main"),
            "{out}"
        );
        let t = fx.sup.row("T1-m").unwrap();
        assert_eq!(t.state, State::Blocked);
        assert!(t.block.unwrap().reason.contains("main checkout is on other, not main"));
        assert!(lock_holders(&fx).is_empty(), "the lock is released");
        assert_eq!(fx.launcher.prompts("s1").len(), before, "the task is told nothing");
        assert_eq!(git(&app, &["branch", "--show-current"]).unwrap(), "other");
    }

    #[tokio::test]
    async fn a_dirty_main_checkout_fails_the_merge_without_touching_it() {
        let fx = fixture(5, &[Some("Merge")]);
        let app = repo(&fx.root, "app");
        run(&fx, spawn_args("app", "m", true)).await.unwrap();
        set_state(&fx, "T1-m", State::Ready);
        std::fs::write(app.join("sub/f"), "user's edit").unwrap();
        let before = fx.launcher.prompts("s1").len();
        let out = run(&fx, json!({"action": "merge", "task": "T1-m"})).await.unwrap();
        assert!(
            out.contains("merge: failed") && out.contains("main checkout has uncommitted changes"),
            "{out}"
        );
        let t = fx.sup.row("T1-m").unwrap();
        assert_eq!(t.state, State::Blocked);
        assert!(lock_holders(&fx).is_empty(), "the lock is released");
        assert_eq!(fx.launcher.prompts("s1").len(), before, "the task is told nothing");
        assert_eq!(std::fs::read_to_string(app.join("sub/f")).unwrap(), "user's edit");
    }

    #[tokio::test]
    async fn one_merge_per_repo_and_non_git_tasks_get_the_close_offer() {
        let fx = fixture(5, &[Some("Merge")]);
        repo(&fx.root, "app");
        run(&fx, spawn_args("app", "a", true)).await.unwrap();
        run(&fx, spawn_args("app", "b", true)).await.unwrap();
        set_state(&fx, "T1-a", State::Ready);
        set_state(&fx, "T2-b", State::Ready);
        run(&fx, json!({"action": "merge", "task": "T1-a"})).await.unwrap();
        assert_eq!(state(&fx, "T1-a"), State::Merging);
        assert_eq!(lock_holders(&fx), ["T1-a"]);
        let e = run(&fx, json!({"action": "merge", "task": "T2-b"})).await.unwrap_err();
        assert!(e.contains("T1-a is merging"), "{e}");
        assert_eq!(fx.asker.asked.lock().unwrap().len(), 1, "no card for the refused merge");
        // Outside git there is nothing to merge: super offers to close instead.
        std::fs::create_dir(fx.root.join("plain")).unwrap();
        run(&fx, spawn_args("plain", "c", true)).await.unwrap();
        set_state(&fx, "T3-c", State::Ready);
        let e = run(&fx, json!({"action": "merge", "task": "T3-c"})).await.unwrap_err();
        assert!(e.contains("not in a git repo") && e.contains("task close"), "{e}");
    }

    #[tokio::test]
    async fn the_merge_asks_for_a_re_check_when_the_base_moved() {
        let fx = fixture(5, &[Some("Merge"), Some("Merge")]);
        let app = repo(&fx.root, "app");
        run(&fx, spawn_args("app", "a", true)).await.unwrap();
        set_state(&fx, "T1-a", State::Ready);
        run(&fx, json!({"action": "merge", "task": "T1-a"})).await.unwrap();
        let text = fx.launcher.prompts("s1").last().unwrap().clone();
        assert!(!text.contains("moved since"), "{text}");
        assert!(
            text.contains("Untracked files (build output, caches) are not a reason to stop"),
            "{text}"
        );
        // The base gets a new commit after T2 started.
        run(&fx, spawn_args("app", "b", true)).await.unwrap();
        std::fs::write(app.join("sub/g"), "new").unwrap();
        sh(&app, &["add", "."]);
        sh(&app, &["commit", "-qm", "more"]);
        set_state(&fx, "T1-a", State::Ready);
        fx.sup.edit(|b| b.merge_locks.clear()).unwrap();
        set_state(&fx, "T2-b", State::Ready);
        run(&fx, json!({"action": "merge", "task": "T2-b"})).await.unwrap();
        let text = fx.launcher.prompts("s2").last().unwrap().clone();
        assert!(
            text.contains("main moved since your work started") && text.contains("re-run your checks"),
            "{text}"
        );
    }

    #[tokio::test]
    async fn stop_while_merging_releases_the_lock_and_a_half_done_merge_blocks() {
        let fx = fixture(5, &[Some("Merge"), Some("Merge"), Some("Merge")]);
        let app = repo(&fx.root, "app");
        for s in ["a", "b", "c"] {
            run(&fx, spawn_args("app", s, true)).await.unwrap();
        }
        // A clean stop: stopped, lock free.
        set_state(&fx, "T1-a", State::Ready);
        run(&fx, json!({"action": "merge", "task": "T1-a"})).await.unwrap();
        run(&fx, json!({"action": "stop", "task": "T1-a"})).await.unwrap();
        assert_eq!(state(&fx, "T1-a"), State::Stopped);
        assert!(lock_holders(&fx).is_empty());
        // The main checkout is left mid-merge: blocked with the reason, lock free.
        set_state(&fx, "T2-b", State::Ready);
        run(&fx, json!({"action": "merge", "task": "T2-b"})).await.unwrap();
        let head = git(&app, &["rev-parse", "HEAD"]).unwrap();
        std::fs::write(app.join(".git/MERGE_HEAD"), format!("{head}\n")).unwrap();
        let out = run(&fx, json!({"action": "stop", "task": "T2-b"})).await.unwrap();
        let t = fx.sup.row("T2-b").unwrap();
        assert_eq!(t.state, State::Blocked, "{out}");
        assert!(t.block.unwrap().reason.contains("merge in progress"), "{out}");
        assert!(out.contains("half-done"), "{out}");
        assert!(lock_holders(&fx).is_empty());
        std::fs::remove_file(app.join(".git/MERGE_HEAD")).unwrap();
        // Stop while merging with uncommitted changes left in the main checkout: blocked, worktree kept.
        set_state(&fx, "T3-c", State::Ready);
        run(&fx, json!({"action": "merge", "task": "T3-c"})).await.unwrap();
        std::fs::write(app.join("sub/f"), "conflict").unwrap();
        let out = run(&fx, json!({"action": "stop", "task": "T3-c"})).await.unwrap();
        let t = fx.sup.row("T3-c").unwrap();
        assert_eq!(t.state, State::Blocked, "{out}");
        assert!(t.block.unwrap().reason.contains("uncommitted changes"), "{out}");
        assert!(app.join(".worktrees/T3-c").exists(), "{out}");
        assert!(lock_holders(&fx).is_empty());
    }

    #[tokio::test]
    async fn a_nested_repo_inside_super_s_pinned_repo_gets_its_own_worktree_and_merge() {
        let fx = fixture(5, &[Some("Merge"), Some("Close")]);
        // Super runs inside an outer repo (its process pinned it), on another branch;
        // the task folder `app` is its own repo, untracked in the outer one.
        let outer = repo(&fx.root, "");
        sh(&outer, &["checkout", "-qb", "outer"]);
        novacode_base::git::pin_repo(&outer).unwrap();
        let app = repo(&fx.root, "app");
        let out = run(&fx, spawn_args("app/sub", "n", true)).await.unwrap();
        let t = fx.sup.row("T1-n").unwrap();
        let wt = app.join(".worktrees/T1-n");
        assert_eq!(t.repo.as_deref(), Some(app.as_path()), "{out}");
        assert_eq!(t.worktree.as_deref(), Some(wt.as_path()), "{out}");
        assert_eq!(t.base_branch.as_deref(), Some("main"), "{out}");
        assert!(wt.join("sub/f").exists());
        assert!(!outer.join(".worktrees").exists(), "nothing in the outer repo");
        assert!(git(&app, &["rev-parse", "--verify", "refs/heads/worktree-T1-n"]).is_ok());
        // Merge and the half-done check see the nested repo.
        set_state(&fx, "T1-n", State::Ready);
        run(&fx, json!({"action": "merge", "task": "T1-n"})).await.unwrap();
        assert_eq!(state(&fx, "T1-n"), State::Merging);
        let head = git(&app, &["rev-parse", "HEAD"]).unwrap();
        std::fs::write(app.join(".git/MERGE_HEAD"), format!("{head}\n")).unwrap();
        let out = run(&fx, json!({"action": "stop", "task": "T1-n"})).await.unwrap();
        assert_eq!(state(&fx, "T1-n"), State::Blocked, "{out}");
        std::fs::remove_file(app.join(".git/MERGE_HEAD")).unwrap();
        // Close sees the nested branch as merged into its base.
        sh(&app, &["merge", "-q", "--ff-only", "worktree-T1-n"]);
        let out = run(&fx, json!({"action": "close", "task": "T1-n"})).await.unwrap();
        assert!(out.contains("Closed T1-n") && !wt.exists(), "{out}");
    }

    #[tokio::test]
    async fn a_reloaded_board_pins_its_task_repos() {
        let fx = fixture(5, &[]);
        let app = repo(&fx.root, "app");
        fx.sup
            .edit(|b| {
                b.tasks.push(Task {
                    id: "T1-r".into(),
                    label: "T1-r".into(),
                    repo: Some(app.clone()),
                    ..Task::default()
                })
            })
            .unwrap();
        assert_eq!(novacode_base::git::pinned(&app), None);
        let cfg = Config {
            max_running: 5,
            tiers: Vec::new(),
            worktree_copy: Vec::new(),
            home: fx.home.clone(),
            allow: fx.sup.cfg.allow.clone(),
        };
        let _again = Super::new(&fx.root, fx.sup.dir.clone(), cfg, None, fx.launcher.clone()).unwrap();
        assert_eq!(novacode_base::git::pinned(&app), Some(app.join(".git")));
    }

    #[tokio::test]
    async fn close_waits_for_the_merge_result_unless_the_merge_was_stopped() {
        let fx = fixture(5, &[Some("Merge"), Some("Close")]);
        repo(&fx.root, "app");
        run(&fx, spawn_args("app", "m", true)).await.unwrap();
        set_state(&fx, "T1-m", State::Ready);
        run(&fx, json!({"action": "merge", "task": "T1-m"})).await.unwrap();
        let e = run(&fx, json!({"action": "close", "task": "T1-m"})).await.unwrap_err();
        assert!(e.contains("wait for the merge result"), "{e}");
        assert_eq!(state(&fx, "T1-m"), State::Merging);
        assert_eq!(fx.asker.asked.lock().unwrap().len(), 1, "no close card");
        run(&fx, json!({"action": "stop", "task": "T1-m"})).await.unwrap();
        let out = run(&fx, json!({"action": "close", "task": "T1-m"})).await.unwrap();
        assert!(out.contains("Closed T1-m"), "{out}");
    }

    #[tokio::test]
    async fn a_goal_task_gets_set_goal_before_its_brief_and_send_after_stop_resumes_it() {
        let fx = fixture(5, &[]);
        std::fs::create_dir(fx.root.join("p")).unwrap();
        let mut a = spawn_args("p", "faster", false);
        a["goal"] = json!(true);
        run(&fx, a).await.unwrap();
        run(&fx, spawn_args("p", "plain", false)).await.unwrap();
        let sent = fx.launcher.sent.lock().unwrap().clone();
        let s1: Vec<&ClientCmd> = sent.iter().filter(|(s, _)| s == "s1").map(|(_, c)| c).collect();
        assert!(
            matches!(s1[0], ClientCmd::SetGoal { objective, criteria }
                if objective.contains("do faster") && criteria == &["it works".to_string()]),
            "{s1:?}"
        );
        assert!(matches!(s1[1], ClientCmd::Prompt { raw: true, .. }), "{s1:?}");
        assert!(
            !sent
                .iter()
                .any(|(s, c)| s == "s2" && matches!(c, ClientCmd::SetGoal { .. })),
            "no goal for a task with a clear result"
        );
        // Stop pauses the goal (the task's goal file says so); a send resumes it first.
        run(&fx, json!({"action": "stop", "task": "T1-faster"})).await.unwrap();
        std::fs::create_dir_all(fx.home.join("goals")).unwrap();
        std::fs::write(fx.home.join("goals/s1.json"), r#"{"state": "paused"}"#).unwrap();
        run(
            &fx,
            json!({"action": "send", "task": "T1-faster", "message": "continue"}),
        )
        .await
        .unwrap();
        let sent = fx.launcher.sent.lock().unwrap().clone();
        let tail: Vec<&ClientCmd> = sent.iter().rev().take(2).map(|(_, c)| c).collect();
        assert!(
            matches!(tail[1], ClientCmd::Prompt { text, raw: false } if text == "/goal resume"),
            "{tail:?}"
        );
        assert!(
            matches!(tail[0], ClientCmd::Prompt { text, raw: true } if text == "continue"),
            "{tail:?}"
        );
    }

    #[test]
    fn preamble_names_the_task_and_the_restart_rule() {
        let p = preamble("T3-fix");
        assert!(p.contains("T3-fix") && p.contains("`report`") && p.contains("check your folder and git"));
    }

    /// B10: the proof log is kept with the session (report.rs), so nothing says it is empty.
    #[test]
    fn no_prompt_says_the_proof_log_is_empty_after_a_restart() {
        assert!(!preamble("T3-fix").contains("proof log is empty"));
        assert!(!super::super::link::RESUMED.contains("proof log is empty"));
        assert!(super::super::link::RESUMED.contains("check your folder and git"));
    }

    /// B1: a start that fails goes through the table: a merging task's lock is released,
    /// and a question to a ready task keeps its state.
    #[tokio::test]
    async fn a_failed_resume_releases_the_merge_lock_and_a_failed_question_keeps_the_state() {
        let fx = fixture(5, &[Some("Merge")]);
        repo(&fx.root, "app");
        run(&fx, spawn_args("app", "m", true)).await.unwrap();
        run(&fx, spawn_args("app", "q", false)).await.unwrap();
        set_state(&fx, "T1-m", State::Ready);
        set_state(&fx, "T2-q", State::Ready);
        fx.launcher.alive.lock().unwrap().clear();
        *fx.launcher.fail_start.lock().unwrap() = true;
        let e = run(&fx, json!({"action": "merge", "task": "T1-m"})).await.unwrap_err();
        assert!(e.contains("no login"), "{e}");
        let t = fx.sup.row("T1-m").unwrap();
        assert_eq!(t.state, State::Failed);
        assert!(t.summary.unwrap().contains("no login"));
        assert!(lock_holders(&fx).is_empty(), "the merge lock is released");
        let ask = json!({"action": "send", "task": "T2-q", "message": "why?", "question": true});
        let e = run(&fx, ask).await.unwrap_err();
        assert!(e.contains("no login"), "{e}");
        assert_eq!(state(&fx, "T2-q"), State::Ready);
    }

    /// B2: a child that never prints its session id is killed after the wait.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_task_that_never_prints_its_session_id_is_killed_after_the_wait() {
        use std::time::{Duration, Instant};
        let mut c = std::process::Command::new("sh");
        c.args(["-c", "sleep 30; echo late"])
            .stdout(std::process::Stdio::piped());
        novacode_base::platform::own_group(&mut c);
        let child = c.spawn().unwrap();
        let pid = child.id();
        let t = Instant::now();
        let e = session_id(child, Duration::from_millis(300)).unwrap_err();
        assert!(e.contains("didn't start within"), "{e}");
        assert!(t.elapsed() < Duration::from_secs(10));
        assert!(!Path::new(&format!("/proc/{pid}")).exists(), "the child is gone");
        let mut c = std::process::Command::new("sh");
        c.args(["-c", "echo abc"]).stdout(std::process::Stdio::piped());
        assert_eq!(session_id(c.spawn().unwrap(), Duration::from_secs(10)).unwrap(), "abc");
    }

    /// B3: a read task that entered a worktree and reported `write` gets the worktree,
    /// branch and base a write task's start records, and its done files.
    #[tokio::test]
    async fn a_read_task_that_enters_a_worktree_gets_its_worktree_branch_and_base() {
        let fx = fixture(5, &[]);
        let app = repo(&fx.root, "app");
        run(&fx, spawn_args("app", "look", false)).await.unwrap();
        let base = git(&app, &["rev-parse", "HEAD"]).unwrap();
        let wt = app.join(".worktrees/look");
        sh(
            &app,
            &["worktree", "add", "-q", "-b", "worktree-look", wt.to_str().unwrap()],
        );
        std::fs::write(wt.join("new"), "y").unwrap();
        sh(&wt, &["add", "new"]);
        sh(&wt, &["commit", "-qm", "work"]);
        let head = git(&wt, &["rev-parse", "HEAD"]).unwrap();
        fx.sup
            .edit(|b| {
                b.tasks[0].done = Some(super::super::board::Done {
                    commit: Some(head),
                    ..Default::default()
                })
            })
            .unwrap();
        fx.sup.became_write("T1-look", Some("worktree-look")).unwrap();
        let t = fx.sup.row("T1-look").unwrap();
        assert!(t.write);
        assert_eq!(t.worktree.as_deref(), Some(wt.as_path()));
        assert_eq!(t.branch.as_deref(), Some("worktree-look"));
        assert_eq!(t.base_branch.as_deref(), Some("main"));
        assert_eq!(t.base_commit.as_deref(), Some(base.as_str()));
        assert_eq!(t.done.unwrap().files, ["new"]);
    }

    /// B4: close stops the review that runs when it applies, not the one seen before its card.
    #[tokio::test]
    async fn close_stops_the_review_running_when_it_applies() {
        let fx = fixture(5, &[Some("Close")]);
        std::fs::create_dir(fx.root.join("p")).unwrap();
        run(&fx, spawn_args("p", "q", false)).await.unwrap();
        set_state(&fx, "T1-q", State::Reviewing);
        let (sup, launcher) = (Arc::downgrade(&fx.sup), fx.launcher.clone());
        *fx.asker.hook.lock().unwrap() = Some(Box::new(move || {
            launcher.alive.lock().unwrap().insert("s-rv".into());
            let sup = sup.upgrade().unwrap();
            sup.edit(|b| {
                b.tasks.push(Task {
                    id: "T2-review-q".into(),
                    label: "T2-review-q".into(),
                    kind: Kind::Review,
                    reviewer_of: Some("T1-q".into()),
                    state: State::Running,
                    session: Some("s-rv".into()),
                    ..Task::default()
                });
                b.tasks[0].reviewer = Some("T2-review-q".into());
            })
            .unwrap();
        }));
        run(&fx, json!({"action": "close", "task": "T1-q"})).await.unwrap();
        assert_eq!(state(&fx, "T1-q"), State::Archived);
        assert_eq!(state(&fx, "T2-review-q"), State::Archived, "the review is stopped");
    }

    /// B6: a row added while the trust card is open doesn't share the new task's number.
    #[tokio::test]
    async fn the_label_number_is_taken_when_the_row_is_added() {
        let fx = fixture(5, &[Some("Trust for this task only")]);
        let app = repo(&fx.root, "app");
        std::fs::write(app.join(".mcp.json"), r#"{"mcpServers": {}}"#).unwrap();
        let sup = Arc::downgrade(&fx.sup);
        *fx.asker.hook.lock().unwrap() = Some(Box::new(move || {
            sup.upgrade()
                .unwrap()
                .edit(|b| {
                    b.tasks.push(Task {
                        id: "T1-review-x".into(),
                        label: "T1-review-x".into(),
                        kind: Kind::Review,
                        state: State::Archived,
                        ..Task::default()
                    })
                })
                .unwrap();
        }));
        let out = run(&fx, spawn_args("app", "one", true)).await.unwrap();
        assert!(out.contains("T2-one"), "{out}");
        assert!(fx.sup.row("T2-one").is_ok());
    }

    /// B7: a write task in a git submodule is refused (its main checkout is not `<common>/..`);
    /// a read task runs there in place.
    #[tokio::test]
    async fn a_write_task_in_a_submodule_is_refused() {
        let fx = fixture(5, &[]);
        let lib = repo(&fx.home, "lib");
        let app = repo(&fx.root, "app");
        sh(
            &app,
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "add",
                "-q",
                lib.to_str().unwrap(),
                "lib",
            ],
        );
        let e = run(&fx, spawn_args("app/lib", "x", true)).await.unwrap_err();
        assert!(e.contains("submodule"), "{e}");
        assert!(fx.launcher.starts.lock().unwrap().is_empty());
        run(&fx, spawn_args("app/lib", "y", false)).await.unwrap();
        let info = run(&fx, json!({"action": "info", "folder": "app/lib"})).await.unwrap();
        assert!(info.contains("submodule"), "{info}");
    }

    /// B9: `task info` shows, before any spawn, whether the folder is in git, the branch
    /// a write task's worktree starts from, and trust; it changes nothing.
    #[tokio::test]
    async fn info_shows_the_base_branch_and_trust_without_changing_anything() {
        let fx = fixture(5, &[]);
        let app = repo(&fx.root, "app");
        let out = run(&fx, json!({"action": "info", "folder": "app/sub"})).await.unwrap();
        assert!(out.contains("git repo") && out.contains("branch main"), "{out}");
        assert!(out.contains("no project config"), "{out}");
        std::fs::write(app.join(".mcp.json"), r#"{"mcpServers": {}}"#).unwrap();
        let out = run(&fx, json!({"action": "info", "folder": "app"})).await.unwrap();
        assert!(out.contains("not trusted") && out.contains("card"), "{out}");
        std::fs::create_dir(fx.root.join("plain")).unwrap();
        let out = run(&fx, json!({"action": "info", "folder": "plain"})).await.unwrap();
        assert!(out.contains("not in git"), "{out}");
        let e = run(&fx, json!({"action": "info", "folder": "../home"}))
            .await
            .unwrap_err();
        assert!(e.contains("outside the super root"), "{e}");
        assert!(run(&fx, json!({"action": "info"})).await.is_err());
        assert!(fx.sup.board.lock().unwrap().tasks.is_empty());
        assert!(fx.asker.asked.lock().unwrap().is_empty());
        assert!(TaskTool(fx.sup.clone()).description().contains("info {folder}"));
    }
}
