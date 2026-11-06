//! Review tasks (L10.9 "Review"): every write task's `done` is judged by a review task
//! on super's current model and thinking, in its own scratch worktree of the reported commit.
//!
//! - **Input:** reasoning-free. The review brief holds the user's request, the criteria,
//!   the scope, the base, the pinned commit, the changed files and the recorded checks;
//!   never the task's summary or transcript.
//! - **Where:** `<super dir>/review/<id>/work` (on disk: builds there can be large), a detached checkout with the
//!   `worktree.copy` files, so the reviewed task's worktree stays clean. Every
//!   worktree under the scratch folder is removed (and `git worktree prune` run) when
//!   the review ends.
//! - **Baseline:** `<super dir>/baseline/<repo hash>/<base commit>.json`, filled lazily
//!   from the reviews' `baseline` fields; code is its only writer.
//!
//! The verdict loop itself (applying verdicts, forwarding findings, crashes, branch
//! moves) runs in [`super::link`]; the row changes are [`super::board::next`]'s.

use super::board::{self, Event, Kind, State, Task, TaskType};
use super::task::Super;
use crate::wire::ClientCmd;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::{Path, PathBuf};

/// The review's scratch folder key: its label plus a piece of its token, so two supers
/// (or two test runs) with the same labels never share a scratch folder.
pub fn scratch_id(rv: &Task) -> String {
    // The token's tail: its head is a time-ordered id, alike for reviews started together.
    let tail = rv.token.len().saturating_sub(8);
    format!("{}-{}", rv.id, rv.token.get(tail..).unwrap_or_default())
}

/// `<super dir>/review/<id>`: the only place a review task may edit.
pub fn scratch(dir: &Path, rv: &Task) -> PathBuf {
    dir.join("review").join(scratch_id(rv))
}

/// At start: remove the scratch folders no live review row owns (a super that went
/// down while retiring a review leaves its scratch worktree behind).
pub(super) fn sweep_scratch(dir: &Path, board: &board::Board) {
    let Ok(entries) = std::fs::read_dir(dir.join("review")) else {
        return;
    };
    let live: Vec<PathBuf> = board
        .tasks
        .iter()
        .filter(|t| t.kind == Kind::Review && t.state != State::Archived)
        .map(|t| scratch(dir, t))
        .collect();
    for e in entries.flatten() {
        let path = e.path();
        if live.contains(&path) {
            continue;
        }
        let name = e.file_name().to_string_lossy().into_owned();
        let repo = board
            .tasks
            .iter()
            .find(|t| t.kind == Kind::Review && scratch_id(t) == name)
            .and_then(|t| t.repo.clone());
        // A crash left it: stop its daemons (codegraph) before removing it, like retire does.
        novacode_tools::process::stop_inside(&path);
        remove_scratch(repo.as_deref(), &path);
    }
}

/// How a base-commit check came out.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum BaseStatus {
    Passes,
    /// Failed in two runs.
    PreExisting,
    /// Failed, then passed on a re-run.
    Flaky,
}

/// One check run on the base commit, keyed by folder (relative to the repo) and command.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BaseCheck {
    #[serde(default)]
    pub folder: String,
    pub command: String,
    pub status: BaseStatus,
    #[serde(default)]
    pub exit_codes: Vec<i64>,
    #[serde(default)]
    pub failing: Vec<String>,
}

/// The baseline file of one repo and base commit.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Baseline {
    #[serde(default)]
    pub checks: Vec<BaseCheck>,
}

/// Passes on the first run; failed twice is pre-existing; failed then passed is flaky.
/// A single failing run proves nothing yet.
fn classify(codes: &[i64]) -> Option<BaseStatus> {
    match codes {
        [0, ..] => Some(BaseStatus::Passes),
        [_, rest @ ..] if rest.contains(&0) => Some(BaseStatus::Flaky),
        [_, _, ..] => Some(BaseStatus::PreExisting),
        _ => None,
    }
}

impl Baseline {
    pub fn path(super_dir: &Path, repo: &Path, commit: &str) -> PathBuf {
        super_dir
            .join("baseline")
            .join(board::prompt_hash(&repo.to_string_lossy()))
            .join(format!("{commit}.json"))
    }

    pub fn load(path: &Path) -> Baseline {
        std::fs::read(path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }

    pub fn find(&self, folder: &str, command: &str) -> Option<&BaseCheck> {
        self.checks.iter().find(|c| c.folder == folder && c.command == command)
    }

    /// Add a review's `baseline` field (`{"checks": [{folder, command, exit_codes, failing}]}`):
    /// classified by code, only checks not recorded yet. Returns how many were added.
    pub fn record(&mut self, field: &Value) -> usize {
        let mut added = 0;
        for c in field["checks"].as_array().into_iter().flatten() {
            let (Some(command), Some(codes)) = (c["command"].as_str(), c["exit_codes"].as_array()) else {
                continue;
            };
            let folder = c["folder"].as_str().unwrap_or("").trim_matches('/');
            let folder = if folder == "." { "" } else { folder };
            let codes: Vec<i64> = codes.iter().filter_map(Value::as_i64).collect();
            let Some(status) = classify(&codes) else { continue };
            if self.find(folder, command).is_some() {
                continue;
            }
            let failing = c["failing"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|f| f.as_str().map(String::from))
                .collect();
            self.checks.push(BaseCheck {
                folder: folder.into(),
                command: command.into(),
                status,
                exit_codes: codes,
                failing,
            });
            added += 1;
        }
        added
    }

    fn lines(&self) -> String {
        self.checks
            .iter()
            .map(|c| {
                let status = match c.status {
                    BaseStatus::Passes => "passes",
                    BaseStatus::PreExisting => "pre-existing failure",
                    BaseStatus::Flaky => "flaky",
                };
                let folder = if c.folder.is_empty() { "." } else { &c.folder };
                let mut l = format!("- `{}` in {folder}: {status}", c.command);
                if !c.failing.is_empty() {
                    l += &format!(" (failing: {})", c.failing.join(", "));
                }
                l
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// Write a review's baseline results into its base commit's file, then delete the files
/// of base commits no live task uses any more.
pub(super) fn record_baseline(sup: &Super, rv: &Task, field: &Value) {
    let (Some(repo), Some(base)) = (&rv.repo, &rv.base_commit) else {
        return;
    };
    let path = Baseline::path(&sup.dir, repo, base);
    let mut b = Baseline::load(&path);
    if b.record(field) > 0 {
        let write = serde_json::to_vec_pretty(&b)
            .map_err(std::io::Error::other)
            .and_then(|json| {
                std::fs::create_dir_all(path.parent().unwrap_or(&sup.dir))?;
                novacode_base::storage::write_atomic_no_backup(
                    &path,
                    &json,
                    novacode_base::storage::Durability::Durable,
                )
            });
        if let Err(e) = write {
            tracing::warn!(%e, "super: baseline file");
        }
    }
    let live: Vec<PathBuf> = {
        let board = sup.board.lock().unwrap();
        board
            .tasks
            .iter()
            .filter(|t| t.state != State::Archived && t.id != rv.id)
            .filter_map(|t| Some(Baseline::path(&sup.dir, t.repo.as_ref()?, t.base_commit.as_ref()?)))
            .collect()
    };
    let Some(dir) = path.parent() else { return };
    for f in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let p = f.path();
        if p != path && p.extension().is_some_and(|e| e == "json") && !live.contains(&p) {
            let _ = std::fs::remove_file(p);
        }
    }
}

/// Scope entries that look like paths (no spaces): a changed file matching none is marked.
fn outside_scope(file: &str, scope: Option<&str>) -> bool {
    let items: Vec<&str> = scope
        .unwrap_or("")
        .split([',', ';'])
        .map(|s| s.trim().trim_end_matches('/'))
        .filter(|s| !s.is_empty() && !s.contains(char::is_whitespace))
        .collect();
    !items.is_empty() && !items.iter().any(|i| file.contains(i))
}

/// What the review brief is built from, besides the reviewed row.
struct BriefIn<'a> {
    label: &'a str,
    cwd: &'a Path,
    scratch: &'a Path,
    note: Option<&'a str>,
    /// A read task's answer (its final message).
    answer: Option<String>,
    baseline: Option<(PathBuf, Baseline)>,
}

/// The review task's first prompt: facts only, never the task's summary or claims.
fn brief(t: &Task, i: &BriefIn) -> String {
    let done = t.done.clone().unwrap_or_default();
    let mut s = format!(
        "Task {}: you are a review task. Review {} ({} task, {}).\n\
You judge the work, not a story about it: you get the user's request, the acceptance criteria and what code \
recorded, never the task's own summary.\n\nThe user's request:\n{}\n",
        i.label,
        t.label,
        t.ty.name(),
        if t.write { "write" } else { "read" },
        t.request.trim()
    );
    if !t.follow_ups.is_empty() {
        s += "\nFollow-ups super sent (on the user's behalf) later (part of the request):\n";
        for f in &t.follow_ups {
            s += &format!("- {}\n", f.trim());
        }
    }
    if !t.asked.is_empty() {
        s += "\nRecorded questions and answers (code's record, not the task's words): cards the task opened, \
relayed to the user, with the answer given:\n";
        for a in &t.asked {
            let options = match a.options.is_empty() {
                true => String::new(),
                false => format!(" (options: {})", a.options.join(", ")),
            };
            s += &format!(
                "- {} (unix time {}): {}{options}; answered: {}\n",
                a.kind, a.at, a.text, a.answer
            );
        }
    }
    if !t.notes.is_empty() {
        s += "\nThe user's recorded decisions for this task (its charter, code's record):\n";
        for n in &t.notes {
            s += &format!("- {n}\n");
        }
    }
    if !t.criteria.is_empty() {
        s += "\nAcceptance criteria: check each one and name it in your findings as met or not met, with its proof.\n";
        for (n, c) in t.criteria.iter().enumerate() {
            s += &format!("{}. {c}\n", n + 1);
        }
    }
    if let Some(scope) = &t.scope {
        s += &format!("\nScope (the areas the change should touch): {scope}\n");
    }
    let base = t.base_commit.as_deref();
    if let Some(commit) = &done.commit {
        s += &format!(
            "\nBase: {} at commit {}. Commit under review: {commit}{}. Your scratch worktree {} is checked out at it; \
the task's own worktree is not yours.\n",
            t.base_branch.as_deref().unwrap_or("(detached)"),
            base.unwrap_or("?"),
            t.branch
                .as_deref()
                .map(|b| format!(" (branch {b})"))
                .unwrap_or_default(),
            i.cwd.display()
        );
    }
    if done.from_commits {
        s += &format!(
            "\n{} sent no report and no proof: it ended its turns without one, so code took the commit and the \
changed files from its branch. No check was recorded: run the checks yourself (the tests and builds the criteria \
need) before any verdict.\n",
            t.label
        );
    }
    if !done.files.is_empty() {
        s += "\nChanged files: justify or reject each one against the request; an unexplained file is a finding.\n";
        for f in &done.files {
            let mark = if outside_scope(f, t.scope.as_deref()) {
                " (outside the scope)"
            } else {
                ""
            };
            s += &format!("- {f}{mark}\n");
        }
    }
    if !done.checks.is_empty() {
        s += "\nRecorded checks: each check the task cited, as code recorded it when it ran (command, exit code, \
whether it ran before the task's last edit, the end of its output); not the task's words. Re-run the ones that \
matter, don't take them on trust.\n";
        for c in &done.checks {
            s += &format!("{c}\n");
        }
    }
    if !done.artifacts.is_empty() {
        s += "\nScreenshots and URLs it cited: open them.\n";
        for a in &done.artifacts {
            s += &format!("- {a}\n");
        }
    }
    if let Some(answer) = &i.answer {
        s += &format!(
            "\nThe task's answer:\n{answer}\n\nCheck the answer against the request and the code. Edit nothing.\n"
        );
    }
    if let (Some(_), Some(base)) = (&done.commit, base) {
        let scratch_base = i.scratch.join("base");
        s += &format!(
            "\nAlso check that nothing else broke (run the tests and builds of the changed code and its callers; the \
full suite when the impact is unclear) and that there is no new bug (read `git diff {base}...HEAD`).\n"
        );
        if t.ty == TaskType::Bugfix {
            s += &format!(
                "\nThis is a bug fix: prove red then green. Make a base checkout with `git worktree add --detach {} {base}`, \
copy the branch's added or changed test files into it, and run the new test there: it must fail on the base and \
pass here. No such test or reproduction is a finding.\n",
                scratch_base.display()
            );
        }
        if let Some((path, b)) = &i.baseline {
            s += &format!(
                "\nBaseline: when a check fails here, look it up for the base commit first. Known (file {}):\n{}\n\
Only when it is missing, run that one command on the base commit in {} (again if it fails) and put the results in \
your report's `baseline` field: {{\"checks\": [{{\"folder\": \"<relative to the repo>\", \"command\": \"...\", \
\"exit_codes\": [1, 1], \"failing\": [\"<test names>\"]}}]}}. A failure that also fails on the base is pre-existing, \
not a break; a flaky one is reported as flaky.\n",
                path.display(),
                if b.checks.is_empty() {
                    "- none recorded yet".to_string()
                } else {
                    b.lines()
                },
                scratch_base.display()
            );
        }
    }
    if let Some(note) = i.note {
        s += &format!("\nThe super agent asks you to look at: {note}\n");
    }
    s += &format!(
        "\nEdit only under {} (scratch scripts, the base checkout); never in the task's worktree or the main checkout, \
also not with bash. Report with `report`: `verdict` approved | needs-proof | rejected, and at most 5 `findings` \
(for needs-proof, concrete proof requests such as a test for a named case or a browser check with a screenshot; \
you may name skills for the fix, such as tdd or diagnosing-bugs). `approved` is refused unless the findings name \
every acceptance criterion by number, one short line each, such as \"Criterion 1: met, `cargo test` passes\"; \
several lines may share a finding (1000 characters in all). A criterion that is not met, for any reason, \
means `rejected` or `needs-proof`, never `approved`; super asks the user when a criterion can't be met.\n",
        i.scratch.display()
    );
    s
}

/// Remove every worktree under `scratch` (`--force`), prune the repo's worktree list,
/// then the folder.
pub(super) fn remove_scratch(repo: Option<&Path>, scratch: &Path) {
    if let Some(repo) = repo {
        let real = novacode_base::platform::canonicalize(scratch).unwrap_or_else(|_| scratch.to_path_buf());
        let listed = super::task::git(repo, &["worktree", "list", "--porcelain"]).unwrap_or_default();
        for p in listed.lines().filter_map(|l| l.strip_prefix("worktree ")) {
            if Path::new(p).starts_with(scratch) || Path::new(p).starts_with(&real) {
                let _ = super::task::git(repo, &["worktree", "remove", "--force", p]);
            }
        }
        let _ = super::task::git(repo, &["worktree", "prune"]);
    }
    let _ = std::fs::remove_dir_all(scratch);
}

impl Super {
    /// Queue a review of `id`, which just moved to `reviewing` with `Effect::StartReview`:
    /// a review row on super's current model and thinking, the reviewed row's `reviewer`
    /// set. Independent context and evidence gates remain the same. When the
    /// review's model can't be used the reviewed task goes to `blocked`; `Err` says why.
    pub(super) fn open_review(&self, id: &str, note: Option<&str>) -> Result<String, String> {
        let t = self.row(id)?;
        let picked = self.cfg().models.review(&self.current_review_model());
        let model = match picked {
            Ok(m) => m,
            Err(e) => {
                let why = format!("its review can't start: {e}");
                self.edit(|b| b.apply(id, Event::ReviewCantStart(why.clone())))??;
                return Err(why);
            }
        };
        let label = {
            let b = self.board.lock().unwrap();
            let n = b.tasks.iter().map(|t| super::task::number(&t.label)).max().unwrap_or(0) + 1;
            format!("T{n}-review-{}", t.label)
        };
        let now = board::now();
        let mut rv = Task {
            id: label.clone(),
            label: label.clone(),
            kind: Kind::Review,
            ty: t.ty,
            folder: t.folder.clone(),
            repo: t.repo.clone(),
            base_branch: t.base_branch.clone(),
            base_commit: t.base_commit.clone(),
            tier: None,
            model,
            trusted: t.trusted,
            token: super::task::new_token(),
            criteria: t.criteria.clone(),
            scope: t.scope.clone(),
            reviewer_of: Some(id.to_string()),
            state: State::Queued,
            started: now,
            updated: now,
            ..Task::default()
        };
        let scratch = scratch(&self.dir, &rv);
        let cwd = self.review_cwd_plan(&t, &scratch);
        let baseline = match (&t.repo, &t.base_commit, t.done.as_ref().and_then(|d| d.commit.as_ref())) {
            (Some(repo), Some(base), Some(_)) => {
                let path = Baseline::path(&self.dir, repo, base);
                let b = Baseline::load(&path);
                Some((path, b))
            }
            _ => None,
        };
        let answer = (!t.write).then(|| self.read(&t, 0, 400).ok()).flatten();
        rv.request = brief(
            &t,
            &BriefIn {
                label: &label,
                cwd: &cwd,
                scratch: &scratch,
                note,
                answer,
                baseline,
            },
        );
        self.edit(|b| {
            b.tasks.push(rv);
            if let Some(r) = b.tasks.iter_mut().find(|r| r.id == id) {
                r.reviewer = Some(label.clone());
            }
        })?;
        Ok(label)
    }

    /// Where the review of `t` runs: its scratch worktree of the pinned commit (in the
    /// same subfolder), or the read task's own folder.
    fn review_cwd_plan(&self, t: &Task, scratch: &Path) -> PathBuf {
        match (&t.repo, t.done.as_ref().and_then(|d| d.commit.as_ref())) {
            (Some(_), Some(_)) => {
                let rel = match (&t.cwd, &t.worktree) {
                    (Some(c), Some(w)) => c.strip_prefix(w).map(Path::to_path_buf).unwrap_or_default(),
                    _ => PathBuf::new(),
                };
                scratch.join("work").join(rel)
            }
            _ => t.cwd.clone().unwrap_or_else(|| self.root.join(&t.folder)),
        }
    }

    /// Make the review's folder before its process starts: the scratch folder, and for a
    /// pinned commit its detached worktree with the `worktree.copy` files.
    pub(super) fn review_cwd(&self, rv: &Task) -> Result<PathBuf, String> {
        let scratch = scratch(&self.dir, rv);
        let Ok(t) = self.row(rv.reviewer_of.as_deref().unwrap_or_default()) else {
            return Ok(self.root.join(&rv.folder));
        };
        let cwd = self.review_cwd_plan(&t, &scratch);
        if let (Some(main), Some(commit)) = (&t.repo, t.done.as_ref().and_then(|d| d.commit.as_ref())) {
            std::fs::create_dir_all(&scratch).map_err(|e| format!("{}: {e}", scratch.display()))?;
            let work = scratch.join("work");
            if !work.exists() {
                novacode_tools::worktree::add_detached(main, &work, commit, &self.cfg().worktree_copy)?;
            }
            std::fs::create_dir_all(&cwd).map_err(|e| e.to_string())?;
        }
        Ok(cwd)
    }

    /// End a review task: its scratch worktrees go, its row is archived (no card), its
    /// process stops (and whatever it wrote meanwhile goes once it has). Nothing when it
    /// is already archived.
    pub(super) fn retire_review(&self, review: &str) {
        let Ok(rv) = self.row(review) else { return };
        if rv.kind != Kind::Review || rv.state == State::Archived {
            return;
        }
        // Scratch before the row: a crash in between leaves a live row, retired again at restart.
        let dir = scratch(&self.dir, &rv);
        let live = rv.session.clone().filter(|s| self.launcher.alive(s));
        if live.is_none() {
            // What its process started may outlive it (codegraph's daemon) and keep writing there.
            novacode_tools::process::stop_inside(&dir);
            remove_scratch(rv.repo.as_deref(), &dir);
        }
        // A live one's scratch goes once it has shut down and what it started is stopped: a
        // daemon in a removed folder can't be found (a crash meanwhile: `sweep_scratch` at start).
        if let Err(e) = self.edit(|b| b.apply(review, Event::Retire)) {
            tracing::warn!(review, %e, "super: review row");
        }
        if let Some(s) = live {
            let l = self.launcher.clone();
            let repo = rv.repo.clone();
            tokio::spawn(async move {
                let _ = l.send(&s, ClientCmd::Shutdown).await;
                for _ in 0..100 {
                    if !l.alive(&s) {
                        break;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                }
                if dir.exists() {
                    let d = dir.clone();
                    let _ = tokio::task::spawn_blocking(move || novacode_tools::process::stop_inside(&d)).await;
                    remove_scratch(repo.as_deref(), &dir);
                }
            });
        }
    }

    /// The reviewed task's branch no longer points at the commit under review.
    pub(super) fn branch_moved(&self, t: &Task) -> bool {
        let (Some(repo), Some(branch), Some(commit)) =
            (&t.repo, &t.branch, t.done.as_ref().and_then(|d| d.commit.as_ref()))
        else {
            return false;
        };
        super::task::git(repo, &["rev-parse", &format!("refs/heads/{branch}")]).is_ok_and(|head| head != *commit)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::superagent::link::{Inform, Links};
    use crate::superagent::task::{Config, Launcher, Start};
    use futures_util::future::BoxFuture;
    use novacode_core::policy::EditRule;
    use novacode_core::policy::{Approval, ApprovalRequest, Approver};
    use novacode_tools::ask::Asker;
    use novacode_types::{Question, Reply};
    use serde_json::json;
    use std::collections::HashSet;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    #[derive(Default)]
    struct FakeLauncher {
        alive: Mutex<HashSet<String>>,
        starts: Mutex<Vec<Start>>,
        sent: Mutex<Vec<(String, ClientCmd)>>,
    }

    impl Launcher for FakeLauncher {
        fn start(&self, s: Start) -> BoxFuture<'_, Result<String, String>> {
            Box::pin(async move {
                let mut starts = self.starts.lock().unwrap();
                let id = format!("s{}", starts.len() + 1);
                starts.push(s);
                self.alive.lock().unwrap().insert(id.clone());
                Ok(id)
            })
        }
        fn send_as<'a>(&'a self, session: &'a str, _id: &'a str, cmd: ClientCmd) -> BoxFuture<'a, Result<(), String>> {
            Box::pin(async move {
                if matches!(cmd, ClientCmd::Shutdown) {
                    self.alive.lock().unwrap().remove(session);
                }
                self.sent.lock().unwrap().push((session.into(), cmd));
                Ok(())
            })
        }
        fn alive(&self, session: &str) -> bool {
            self.alive.lock().unwrap().contains(session)
        }
    }

    /// Super's cards: records each question and picks `pick` (none: dismissed).
    #[derive(Default)]
    struct Pick {
        pick: Mutex<Option<String>>,
        asked: Mutex<Vec<String>>,
    }
    impl Asker for Pick {
        fn ask(&self, questions: Vec<Question>) -> BoxFuture<'_, Option<Vec<Reply>>> {
            let q = questions[0].question.clone();
            self.asked.lock().unwrap().push(q.clone());
            let pick = self.pick.lock().unwrap().clone();
            Box::pin(async move {
                Some(vec![Reply {
                    question: q,
                    chosen: pick.into_iter().collect(),
                    text: None,
                    preview: None,
                }])
            })
        }
    }

    struct NoAsk;
    impl Asker for NoAsk {
        fn ask(&self, _: Vec<Question>) -> BoxFuture<'_, Option<Vec<Reply>>> {
            Box::pin(async { None })
        }
    }
    struct Allow;
    impl Approver for Allow {
        fn approve(&self, _: ApprovalRequest) -> BoxFuture<'_, Approval> {
            Box::pin(async { Approval::Once })
        }
    }
    #[derive(Default)]
    struct Said(Mutex<Vec<String>>);
    impl Inform for Said {
        fn wake(&self, t: String) {
            self.0.lock().unwrap().push(format!("wake: {t}"));
        }
        fn note(&self, t: String) {
            self.0.lock().unwrap().push(format!("note: {t}"));
        }
        fn notice(&self, _: String) {}
        fn desktop(&self, _: String, _: String) {}
    }

    struct Fx {
        _tmp: tempfile::TempDir,
        app: PathBuf,
        sup: Arc<Super>,
        links: Arc<Links>,
        launcher: Arc<FakeLauncher>,
        said: Arc<Said>,
        cards: Arc<Pick>,
    }

    fn sh(dir: &Path, args: &[&str]) -> String {
        let o = std::process::Command::new("git")
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
            .unwrap();
        assert!(
            o.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&o.stderr)
        );
        String::from_utf8_lossy(&o.stdout).trim().to_string()
    }

    /// A super over a root holding the repo `app` (branch `main`, untracked `AGENTS.md`).
    fn fixture(review_model: &str) -> Fx {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().canonicalize().unwrap();
        let (root, home) = (base.join("root"), base.join("home"));
        let app = root.join("app");
        std::fs::create_dir_all(&app).unwrap();
        std::fs::write(app.join("lib.rs"), "fn a() {}\n").unwrap();
        sh(&app, &["init", "-q", "-b", "main"]);
        sh(&app, &["add", "."]);
        sh(&app, &["commit", "-qm", "init"]);
        std::fs::write(app.join("AGENTS.md"), "rules").unwrap();
        let cfg = Config {
            max_running: 5,
            models: Arc::new(super::super::models::tests::flat_with(review_model)),
            model: "faux/script".into(),
            thinking: novacode_types::ThinkingLevel::Medium,
            worktree_copy: vec!["AGENTS.md".into()],
            home: home.clone(),
            watch: Default::default(),
        };
        let launcher = Arc::new(FakeLauncher::default());
        let cards = Arc::new(Pick::default());
        let sup =
            Arc::new(Super::new(&root, home.join("super/x"), cfg, Some(cards.clone()), launcher.clone()).unwrap());
        let said = Arc::new(Said::default());
        let links = Links::start(&sup, Arc::new(NoAsk), Arc::new(Allow), said.clone());
        Fx {
            _tmp: tmp,
            app,
            sup,
            links,
            launcher,
            said,
            cards,
        }
    }

    impl Fx {
        async fn run(&self, args: Value) -> Result<String, String> {
            self.sup.run(args).await
        }

        /// Spawn a write task on `tier` in `app`; its worktree.
        async fn spawn(&self, slug: &str, tier: &str, ty: &str) -> PathBuf {
            self.run(json!({
                "action": "spawn", "request": format!("please {slug}"), "folder": "app", "slug": slug,
                "write": true, "type": ty, "tier": tier, "criteria": ["the parser accepts tabs"],
                "scope": "src/, tests/"
            }))
            .await
            .unwrap();
            self.sup
                .row(&format!("T1-{slug}"))
                .map(|t| t.worktree.unwrap())
                .unwrap()
        }

        /// Commit `file` in `wt`; the new HEAD.
        fn commit(&self, wt: &Path, file: &str) -> String {
            let p = wt.join(file);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, format!("{file} {}\n", board::now())).unwrap();
            sh(wt, &["add", file]);
            sh(wt, &["commit", "-qm", file]);
            sh(wt, &["rev-parse", "HEAD"])
        }

        fn report(&self, task: &str, fields: Value) {
            static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
            let t = self.sup.row(task).unwrap();
            let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let dir = self.sup.dir.join("inbox");
            std::fs::create_dir_all(&dir).unwrap();
            let mut r = json!({"id": format!("{task}-{}", novacode_base::new_id()), "task": task, "seq": seq, "token": t.token});
            r.as_object_mut().unwrap().extend(fields.as_object().unwrap().clone());
            std::fs::write(dir.join(format!("{task}-{seq}.json")), r.to_string()).unwrap();
            self.links.kick();
        }

        fn done(&self, task: &str, commit: &str, files: &[&str]) {
            self.report(
                task,
                json!({"state": "done", "summary": "SECRET SUMMARY trust me", "commit": commit, "write": true,
                       "files_changed": files, "artifacts": ["/tmp/shot.png"],
                       "checks": [{"n": 3, "command": "cargo test -p app", "folder": "/w", "exit_code": 0, "last_line": "CLAIM ok",
                                   "before_last_edit": true,
                                   "tail": "running 3 tests\ntest result: ok. 3 passed"}]}),
            );
        }

        fn row(&self, id: &str) -> Task {
            self.sup.row(id).unwrap()
        }

        fn said(&self) -> Vec<String> {
            self.said.0.lock().unwrap().clone()
        }

        fn prompts(&self, session: &str) -> Vec<String> {
            self.launcher
                .sent
                .lock()
                .unwrap()
                .iter()
                .filter(|(s, _)| s == session)
                .filter_map(|(_, c)| match c {
                    ClientCmd::Prompt { text, .. } => Some(text.clone()),
                    _ => None,
                })
                .collect()
        }

        async fn until(&self, what: &str, f: impl Fn(&Fx) -> bool) {
            for _ in 0..500 {
                if f(self) {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            panic!("never: {what}; said {:?}", self.said());
        }

        /// The running review of `id`, once it started.
        async fn review_of(&self, id: &str) -> Task {
            self.until("a running review", |fx| {
                fx.row(id).reviewer.is_some_and(|r| fx.row(&r).state == State::Running)
            })
            .await;
            self.row(&self.row(id).reviewer.unwrap())
        }
    }

    fn worktrees(repo: &Path) -> String {
        sh(repo, &["worktree", "list", "--porcelain"])
    }

    #[tokio::test]
    async fn new_reviews_follow_model_and_thinking_changes_without_changing_work() {
        use novacode_types::ThinkingLevel;
        let fx = fixture("denied/legacy-review");
        let wt = fx.spawn("fix", "deep", "change").await;
        let commit = fx.commit(&wt, "src/a.rs");
        let worker = fx.row("T1-fix").model;
        fx.sup.set_current_model("faux/current".into(), ThinkingLevel::Low);
        fx.done("T1-fix", &commit, &["src/a.rs"]);
        let first = fx.review_of("T1-fix").await;
        assert_eq!(first.model, "faux/current:low");
        fx.sup.set_current_model("faux/current".into(), ThinkingLevel::High);
        assert_eq!(fx.row(&first.id).model, "faux/current:low", "live reviews stay pinned");
        fx.report(&first.id, json!({"verdict": "approved", "findings": []}));
        fx.until("ready", |fx| fx.row("T1-fix").state == State::Ready).await;
        fx.run(json!({"action": "review", "task": "T1-fix"})).await.unwrap();
        let second = fx.review_of("T1-fix").await;
        assert_eq!(second.model, "faux/current:high", "thinking-only change");
        fx.report(&second.id, json!({"verdict": "approved", "findings": []}));
        fx.until("ready", |fx| fx.row("T1-fix").state == State::Ready).await;
        fx.sup.set_current_model("other/current".into(), ThinkingLevel::Off);
        fx.run(json!({"action": "review", "task": "T1-fix"})).await.unwrap();
        assert_eq!(fx.review_of("T1-fix").await.model, "other/current:off");
        assert_eq!(fx.row("T1-fix").model, worker);
    }

    #[test]
    fn baseline_failing_twice_is_pre_existing_and_passing_on_a_rerun_is_flaky() {
        let mut b = Baseline::default();
        let field = json!({"checks": [
            {"folder": "crates/a", "command": "cargo test", "exit_codes": [101, 101], "failing": ["t::x"]},
            {"folder": ".", "command": "cargo test", "exit_codes": [1, 0]},
            {"folder": "", "command": "make", "exit_codes": [0]},
            {"folder": "", "command": "once", "exit_codes": [1]}
        ]});
        assert_eq!(b.record(&field), 3, "a single failing run proves nothing");
        assert_eq!(
            b.find("crates/a", "cargo test").unwrap().status,
            BaseStatus::PreExisting
        );
        assert_eq!(
            b.find("", "cargo test").unwrap().status,
            BaseStatus::Flaky,
            "same command, other folder"
        );
        assert_eq!(b.find("", "make").unwrap().status, BaseStatus::Passes);
        // Written once: a later result for the same key doesn't replace it.
        let again = json!({"checks": [{"folder": "", "command": "make", "exit_codes": [2, 2]}]});
        assert_eq!(b.record(&again), 0);
        assert_eq!(b.find("", "make").unwrap().status, BaseStatus::Passes);
    }

    #[tokio::test]
    async fn a_write_task_s_done_starts_a_review_in_a_scratch_worktree_and_approval_makes_it_ready() {
        let fx = fixture("faux/review");
        let wt = fx.spawn("fix", "quick", "change").await;
        let commit = fx.commit(&wt, "src/parse.rs");
        fx.done("T1-fix", &commit, &["src/parse.rs", "build.rs"]);
        let rv = fx.review_of("T1-fix").await;
        assert_eq!(rv.label, "T2-review-T1-fix");
        assert_eq!((rv.kind, rv.model.as_str()), (Kind::Review, "faux/script:medium"));
        assert_eq!(fx.row("T1-fix").state, State::Reviewing);
        let e = fx.run(json!({"action": "accept", "task": "T1-fix"})).await.unwrap_err();
        assert!(e.contains("only through an approved review"), "{e}");

        // Its own scratch worktree of the pinned commit, with the copy list; only it is editable.
        // C4: on disk under super's folder, never /tmp (tmpfs).
        let scratch = scratch(&fx.sup.dir, &rv);
        assert!(
            scratch.starts_with(fx.sup.dir.join("review"))
                && scratch
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with("T2-review-T1-fix-"),
            "{}",
            scratch.display()
        );
        let start = fx.launcher.starts.lock().unwrap()[1].clone();
        assert_eq!(
            start.model, rv.model,
            "the process receives the exact model and thinking"
        );
        assert_eq!(start.cwd, scratch.join("work"));
        assert_eq!(sh(&start.cwd, &["rev-parse", "HEAD"]), commit);
        assert!(start.cwd.join("AGENTS.md").exists() && start.cwd.join("src/parse.rs").exists());
        let rule = start.env.iter().find(|(k, _)| k == EditRule::ENV).unwrap();
        assert_eq!(rule.1, format!("review:{}", scratch.display()));
        assert!(start.env.contains(&("NOVACODE_TASK".into(), rv.id.clone())));

        // Reasoning-free brief: facts, never the task's summary; the checks as code recorded them.
        let brief = fx.prompts("s2").remove(0);
        assert!(
            brief.contains("Recorded checks")
                && brief.contains("- `cargo test -p app` in /w → exit 0 (before the last edit)")
                && brief.contains("running 3 tests\ntest result: ok. 3 passed"),
            "{brief}"
        );
        assert!(brief.contains("Criterion 1"), "the gate is explained: {brief}");
        for want in [
            "please fix",
            "the parser accepts tabs",
            &commit,
            "cargo test -p app",
            "exit 0",
            "/tmp/shot.png",
        ] {
            assert!(brief.contains(want), "{want}: {brief}");
        }
        assert!(
            brief.contains("- build.rs (outside the scope)") && brief.contains("- src/parse.rs\n"),
            "{brief}"
        );
        assert!(!brief.contains("SECRET") && !brief.contains("CLAIM"), "{brief}");
        assert!(!brief.contains("red then green"), "not a bug fix");

        // Build output in the scratch worktree never reaches the task's worktree.
        std::fs::write(start.cwd.join("coverage.out"), "x").unwrap();
        fx.report(
            &rv.id,
            json!({"verdict": "approved", "findings": ["criterion 1 met: test #2"]}),
        );
        fx.until("ready", |fx| fx.row("T1-fix").state == State::Ready).await;
        assert_eq!(fx.row(&rv.id).state, State::Archived, "archived by code");
        assert_eq!(fx.row("T1-fix").reviewer, None);
        // Once the live review has shut down.
        fx.until("scratch removed", |_| !scratch.exists()).await;
        assert!(!worktrees(&fx.app).contains(&scratch_id(&rv)), "{}", worktrees(&fx.app));
        // Clean as `report` and `close` see it (its own `worktree.copy` files don't count).
        assert_eq!(
            novacode_tools::worktree::here(&wt).unwrap().dirty,
            0,
            "the task's worktree stays clean"
        );
        assert!(!wt.join("coverage.out").exists());
        assert!(!fx.launcher.alive("s2"), "the review process is shut down");
        let said = fx.said();
        assert!(
            said.iter().any(
                |l| l.starts_with("wake: T1-fix's review approved it (now ready)") && l.contains("criterion 1 met")
            ),
            "{said:?}"
        );
    }

    #[tokio::test]
    async fn findings_are_forwarded_word_for_word_and_two_rounds_end_in_blocked() {
        let fx = fixture("faux/review");
        let wt = fx.spawn("fix", "standard", "bugfix").await;
        let c1 = fx.commit(&wt, "src/a.rs");
        fx.done("T1-fix", &c1, &["src/a.rs"]);
        let rv = fx.review_of("T1-fix").await;
        assert_eq!(rv.model, "faux/script:medium");
        let brief = fx.prompts("s2").remove(0);
        assert!(
            brief.contains("red then green")
                && brief.contains(&format!("{}/base", scratch(&fx.sup.dir, &rv).display())),
            "{brief}"
        );

        fx.report(
            &rv.id,
            json!({"verdict": "rejected", "findings": ["F1: `parse(\"\\t\")` panics"]}),
        );
        fx.until("running", |fx| fx.row("T1-fix").state == State::Running).await;
        assert_eq!(fx.row("T1-fix").review_rounds, 1);
        let sent = fx.prompts("s1");
        assert!(
            sent.last().unwrap().contains("\n- F1: `parse(\"\\t\")` panics"),
            "{sent:?}"
        );
        assert!(fx.said().iter().any(|l| l.contains("says rejected (round 1 of 2)")));

        let c2 = fx.commit(&wt, "tests/a.rs");
        fx.done("T1-fix", &c2, &["src/a.rs", "tests/a.rs"]);
        fx.until("second review", |fx| {
            fx.row("T1-fix").reviewer.is_some_and(|r| r != rv.id)
        })
        .await;
        let rv2 = fx.review_of("T1-fix").await;
        fx.report(
            &rv2.id,
            json!({"verdict": "needs-proof", "findings": ["show the test failing on the base"]}),
        );
        fx.until("blocked", |fx| fx.row("T1-fix").state == State::Blocked).await;
        let t = fx.row("T1-fix");
        assert_eq!(t.review_rounds, 2);
        assert!(t.block.unwrap().reason.contains("rounds"));
        assert!(
            fx.said()
                .iter()
                .any(|l| l.contains("rounds are used up") && l.contains("show the test failing"))
        );
        assert_eq!(fx.prompts("s1").len(), sent.len(), "nothing more sent to the task");
    }

    #[tokio::test]
    async fn a_task_out_of_review_rounds_is_ready_only_after_the_user_accepts_on_a_card() {
        let fx = fixture("faux/review");
        let wt = fx.spawn("fix", "standard", "change").await;
        for (round, file) in ["src/a.rs", "src/b.rs"].into_iter().enumerate() {
            let c = fx.commit(&wt, file);
            fx.done("T1-fix", &c, &[file]);
            let rv = fx.review_of("T1-fix").await;
            fx.report(&rv.id, json!({"verdict": "rejected", "findings": ["no"]}));
            let want = if round == 0 { State::Running } else { State::Blocked };
            fx.until("the verdict applied", |fx| fx.row("T1-fix").state == want)
                .await;
        }
        let accept = json!({"action": "accept", "task": "T1-fix"});
        // Dismissed or cancelled: it stays blocked.
        let out = fx.run(accept.clone()).await.unwrap();
        assert!(out.contains("declined"), "{out}");
        assert_eq!(fx.row("T1-fix").state, State::Blocked);
        assert!(
            fx.cards
                .asked
                .lock()
                .unwrap()
                .iter()
                .any(|q| q.starts_with("T1-fix · blocked")
                    && q.contains("\n\nAccept T1-fix without an approved review?")),
            "{:?}",
            fx.cards.asked.lock().unwrap()
        );
        *fx.cards.pick.lock().unwrap() = Some("Accept".into());
        let out = fx.run(accept).await.unwrap();
        assert!(out.contains("T1-fix is ready"), "{out}");
        let t = fx.row("T1-fix");
        assert_eq!((t.state, t.accepted), (State::Ready, true));
        let list = fx.run(json!({"action": "list"})).await.unwrap();
        assert!(list.contains("| ready (accepted without review) |"), "{list}");
        assert_eq!(fx.row("T1-fix").reviewer, None, "no new review");
    }

    #[tokio::test]
    async fn a_question_to_a_reviewing_task_keeps_its_review_running() {
        let fx = fixture("faux/review");
        let wt = fx.spawn("fix", "standard", "change").await;
        let c1 = fx.commit(&wt, "src/a.rs");
        fx.done("T1-fix", &c1, &["src/a.rs"]);
        let rv = fx.review_of("T1-fix").await;
        let ask = json!({"action": "send", "task": "T1-fix", "message": "paste check #3", "question": true});
        fx.run(ask).await.unwrap();
        let t = fx.row("T1-fix");
        assert_eq!(
            (t.state, t.reviewer.as_deref()),
            (State::Reviewing, Some(rv.id.as_str()))
        );
        assert_eq!(fx.row(&rv.id).state, State::Running);
        assert!(
            fx.launcher.alive(rv.session.as_deref().unwrap()),
            "the review task lives"
        );
    }

    #[tokio::test]
    async fn the_review_brief_carries_super_s_follow_ups_but_not_code_s_resends() {
        let fx = fixture("faux/review");
        let wt = fx.spawn("fix", "standard", "change").await;
        let send = json!({"action": "send", "task": "T1-fix", "message": "also add a README line"});
        fx.run(send).await.unwrap();
        let c1 = fx.commit(&wt, "README.md");
        fx.done("T1-fix", &c1, &["README.md"]);
        let rv = fx.review_of("T1-fix").await;
        let brief = fx.prompts(&rv.session.clone().unwrap()).remove(0);
        let request = brief.find("please fix").unwrap();
        let follow = brief
            .find("also add a README line")
            .unwrap_or_else(|| panic!("{brief}"));
        assert!(
            request < follow && brief.contains("Follow-ups super sent (on the user's behalf)"),
            "{brief}"
        );
        // The findings code forwards are not follow-ups.
        fx.report(&rv.id, json!({"verdict": "rejected", "findings": ["F1 tabs"]}));
        fx.until("running", |fx| fx.row("T1-fix").state == State::Running).await;
        let c2 = fx.commit(&wt, "src/a.rs");
        fx.done("T1-fix", &c2, &["README.md", "src/a.rs"]);
        fx.until("second review", |fx| {
            fx.row("T1-fix").reviewer.is_some_and(|r| r != rv.id)
        })
        .await;
        let rv2 = fx.review_of("T1-fix").await;
        let brief = fx.prompts(&rv2.session.clone().unwrap()).remove(0);
        assert!(
            brief.contains("also add a README line") && !brief.contains("F1 tabs"),
            "{brief}"
        );
        assert_eq!(fx.row("T1-fix").follow_ups, ["also add a README line"]);
    }

    #[tokio::test]
    async fn the_review_brief_lists_the_recorded_questions_and_answers() {
        let fx = fixture("faux/review");
        let wt = fx.spawn("fix", "standard", "change").await;
        fx.sup
            .edit(|b| {
                let row = b.tasks.iter_mut().find(|r| r.id == "T1-fix").unwrap();
                row.asked.push(board::Asked {
                    kind: "question".into(),
                    text: "which port?".into(),
                    options: vec!["8080".into(), "3000".into()],
                    answer: "8080".into(),
                    at: 1,
                });
            })
            .unwrap();
        let c1 = fx.commit(&wt, "src/a.rs");
        fx.done("T1-fix", &c1, &["src/a.rs"]);
        let rv = fx.review_of("T1-fix").await;
        let brief = fx.prompts(&rv.session.clone().unwrap()).remove(0);
        let at = brief
            .find("Recorded questions and answers (code's record, not the task's words)")
            .unwrap_or_else(|| panic!("{brief}"));
        let rest = &brief[at..];
        assert!(
            rest.contains("which port?") && rest.contains("8080, 3000") && rest.contains("answered: 8080"),
            "{brief}"
        );
    }

    #[tokio::test]
    async fn task_review_on_a_ready_task_reviews_again_without_a_round_and_baselines_are_reused() {
        let fx = fixture("faux/review");
        let wt = fx.spawn("fix", "deep", "change").await;
        let c1 = fx.commit(&wt, "src/a.rs");
        fx.done("T1-fix", &c1, &["src/a.rs"]);
        let rv = fx.review_of("T1-fix").await;
        assert_eq!(rv.model, "faux/script:medium");
        let base = rv.base_commit.clone().unwrap();
        fx.report(
            &rv.id,
            json!({"verdict": "approved", "findings": [], "baseline": {"checks": [
                {"folder": "", "command": "cargo test", "exit_codes": [101, 0], "failing": ["net::timeout"]}]}}),
        );
        fx.until("ready", |fx| fx.row("T1-fix").state == State::Ready).await;
        let file = Baseline::path(&fx.sup.dir, &fx.app, &base);
        assert_eq!(
            Baseline::load(&file).find("", "cargo test").unwrap().status,
            BaseStatus::Flaky
        );

        let rounds = fx.row("T1-fix").review_rounds;
        let out = fx
            .run(json!({"action": "review", "task": "T1-fix", "note": "the findings were empty"}))
            .await
            .unwrap();
        assert!(out.contains("T3-review-T1-fix"), "{out}");
        let t = fx.row("T1-fix");
        assert_eq!((t.state, t.review_rounds), (State::Reviewing, rounds));
        let rv2 = fx.review_of("T1-fix").await;
        let brief = fx.prompts(rv2.session.as_deref().unwrap()).remove(0);
        assert!(brief.contains("the findings were empty"), "{brief}");
        assert!(
            brief.contains("`cargo test` in .: flaky (failing: net::timeout)"),
            "reused: {brief}"
        );
        // The second review uses no round even when it rejects.
        fx.report(&rv2.id, json!({"verdict": "rejected", "findings": ["x"]}));
        fx.until("running", |fx| fx.row("T1-fix").state == State::Running).await;
        assert_eq!(fx.row("T1-fix").review_rounds, rounds);
    }

    /// Block `id` as code does when its turn ends without a report.
    fn block_unreported(fx: &Fx, id: &str) {
        fx.sup.edit(|b| b.apply(id, Event::TurnUnreported)).unwrap().unwrap();
        assert!(fx.row(id).block.unwrap().code_made);
    }

    #[tokio::test]
    async fn task_review_on_a_task_that_never_reported_reviews_its_commits() {
        // R23: committed work, turn after turn without `report`.
        let fx = fixture("faux/review");
        let wt = fx.spawn("fix", "quick", "change").await;
        fx.commit(&wt, "src/a.rs");
        let head = fx.commit(&wt, "tests/a.rs");
        block_unreported(&fx, "T1-fix");
        let out = fx.run(json!({"action": "review", "task": "T1-fix"})).await.unwrap();
        assert!(out.contains("T2-review-T1-fix"), "{out}");
        let t = fx.row("T1-fix");
        assert_eq!((t.state, t.block.clone()), (State::Reviewing, None));
        assert_eq!(t.summary.as_deref(), Some("no report; reviewed from its commits"));
        let done = t.done.unwrap();
        assert_eq!(done.commit.as_deref(), Some(head.as_str()));
        assert_eq!(done.files, ["src/a.rs", "tests/a.rs"]);
        assert!(done.checks.is_empty() && done.artifacts.is_empty());
        let rv = fx.review_of("T1-fix").await;
        assert_eq!(
            sh(&fx.launcher.starts.lock().unwrap()[1].cwd, &["rev-parse", "HEAD"]),
            head
        );
        let brief = fx.prompts(rv.session.as_deref().unwrap()).remove(0);
        assert!(
            brief.contains("T1-fix sent no report and no proof") && brief.contains("run the checks yourself"),
            "{brief}"
        );
        // It uses a round.
        fx.report(&rv.id, json!({"verdict": "rejected", "findings": ["no test"]}));
        fx.until("running", |fx| fx.row("T1-fix").state == State::Running).await;
        assert_eq!(fx.row("T1-fix").review_rounds, 1);
    }

    #[tokio::test]
    async fn task_review_on_an_unreported_task_whose_head_is_approved_makes_it_ready() {
        // L10.9 R27 (7): its approved commit needs no new review.
        let fx = fixture("faux/review");
        let wt = fx.spawn("fix", "quick", "change").await;
        let head = fx.commit(&wt, "src/a.rs");
        block_unreported(&fx, "T1-fix");
        fx.sup
            .edit(|b| b.tasks.iter_mut().find(|t| t.id == "T1-fix").unwrap().approved_commit = Some(head.clone()))
            .unwrap();
        let out = fx.run(json!({"action": "review", "task": "T1-fix"})).await.unwrap();
        let t = fx.row("T1-fix");
        assert_eq!((t.state, t.reviewer), (State::Ready, None), "{out}");
        assert_eq!(t.done.unwrap().commit.as_deref(), Some(head.as_str()));
    }

    #[tokio::test]
    async fn task_review_on_a_task_that_never_reported_nor_committed_is_refused() {
        let fx = fixture("faux/review");
        fx.spawn("fix", "quick", "change").await;
        block_unreported(&fx, "T1-fix");
        let e = fx.run(json!({"action": "review", "task": "T1-fix"})).await.unwrap_err();
        assert_eq!(e, "T1-fix: nothing committed to review; send it on or close it");
        let t = fx.row("T1-fix");
        assert_eq!((t.state, t.done, t.reviewer), (State::Blocked, None, None));
    }

    #[tokio::test]
    async fn task_review_on_a_task_that_reported_blocked_is_refused() {
        let fx = fixture("faux/review");
        let wt = fx.spawn("fix", "quick", "change").await;
        fx.commit(&wt, "src/a.rs");
        fx.report("T1-fix", json!({"state": "blocked", "summary": "need a key"}));
        fx.until("blocked", |fx| fx.row("T1-fix").state == State::Blocked).await;
        let e = fx.run(json!({"action": "review", "task": "T1-fix"})).await.unwrap_err();
        assert!(e.contains("review is not possible while it is blocked"), "{e}");
        let t = fx.row("T1-fix");
        assert_eq!((t.done, t.reviewer), (None, None));
    }

    #[tokio::test]
    async fn a_denied_review_blocks_until_explicit_retry_uses_current_super_model() {
        let fx = fixture("faux/review");
        let wt = fx.spawn("fix", "deep", "change").await;
        fx.sup
            .set_current_model("denied/fable".into(), novacode_types::ThinkingLevel::Medium);
        let c = fx.commit(&wt, "src/a.rs");
        fx.done("T1-fix", &c, &["src/a.rs"]);
        fx.until("blocked", |fx| fx.row("T1-fix").state == State::Blocked).await;
        let t = fx.row("T1-fix");
        assert!(t.block.unwrap().reason.contains("denied/fable is denied"));
        assert_eq!(t.reviewer, None);
        assert_eq!(fx.sup.board.lock().unwrap().tasks.len(), 1, "no review row");
        let said = fx.said();
        assert!(
            said.iter()
                .any(|l| l.starts_with("wake:") && l.contains("review can't start")),
            "{said:?}"
        );
        let recorded = fx.row("T1-fix").done;
        let summary = fx.row("T1-fix").summary;
        fx.sup
            .set_current_model("faux/recovered".into(), novacode_types::ThinkingLevel::Low);
        assert_eq!(fx.row("T1-fix").state, State::Blocked, "no automatic restart");
        fx.run(json!({"action": "review", "task": "T1-fix"})).await.unwrap();
        assert_eq!(fx.review_of("T1-fix").await.model, "faux/recovered:low");
        assert_eq!(fx.row("T1-fix").done, recorded);
        assert_eq!(fx.row("T1-fix").summary, summary);
    }

    #[tokio::test]
    async fn a_crashed_review_is_retried_once_then_the_task_is_blocked_with_the_reason() {
        let fx = fixture("faux/review");
        let wt = fx.spawn("fix", "quick", "change").await;
        let c = fx.commit(&wt, "src/a.rs");
        fx.done("T1-fix", &c, &["src/a.rs"]);
        let rv = fx.review_of("T1-fix").await;
        fx.launcher.alive.lock().unwrap().remove(rv.session.as_deref().unwrap());
        fx.until("a new review", |fx| {
            fx.row("T1-fix").reviewer.is_some_and(|r| r != rv.id)
        })
        .await;
        assert_eq!(fx.row(&rv.id).state, State::Archived);
        assert!(!scratch(&fx.sup.dir, &rv).exists());
        assert_eq!(fx.row("T1-fix").state, State::Reviewing);
        let rv2 = fx.review_of("T1-fix").await;
        fx.launcher
            .alive
            .lock()
            .unwrap()
            .remove(rv2.session.as_deref().unwrap());
        fx.until("blocked", |fx| fx.row("T1-fix").state == State::Blocked).await;
        let reason = fx.row("T1-fix").block.unwrap().reason;
        assert!(
            reason.contains("crashed twice") && reason.contains("its process exited"),
            "{reason}"
        );
        assert!(
            fx.said()
                .iter()
                .any(|l| l.starts_with("wake: T1-fix is blocked") && l.contains("Ask the user"))
        );
        assert!(!worktrees(&fx.app).contains(&fx.sup.dir.join("review").to_string_lossy().to_string()));
    }

    /// C5: a super that went down between archiving a review and removing its scratch
    /// worktree removes it at the next start.
    #[tokio::test]
    async fn an_archived_review_s_scratch_left_by_a_crash_is_removed_at_start() {
        let fx = fixture("faux/review");
        let wt = fx.spawn("fix", "quick", "change").await;
        let c = fx.commit(&wt, "src/a.rs");
        fx.done("T1-fix", &c, &["src/a.rs"]);
        let rv = fx.review_of("T1-fix").await;
        let s = scratch(&fx.sup.dir, &rv);
        assert!(s.join("work").exists());
        // The crash: the row is archived, its scratch worktree is still there.
        fx.sup
            .edit(|b| b.tasks.iter_mut().find(|t| t.id == rv.id).unwrap().state = State::Archived)
            .unwrap();
        let cfg = Config {
            max_running: 1,
            models: Arc::new(super::super::models::tests::flat()),
            model: "faux/script".into(),
            thinking: novacode_types::ThinkingLevel::Medium,
            worktree_copy: vec![],
            home: fx.sup.home().to_path_buf(),
            watch: Default::default(),
        };
        let again = Super::new(&fx.sup.root, fx.sup.dir.clone(), cfg, None, fx.launcher.clone()).unwrap();
        drop(again);
        assert!(!s.exists(), "scratch removed");
        assert!(!worktrees(&fx.app).contains(&scratch_id(&rv)), "{}", worktrees(&fx.app));
    }

    /// L-leak: retiring a live review removed its scratch before stopping what runs there,
    /// so a daemon it started (codegraph's) was never found again and ran on.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn retiring_a_live_review_stops_its_daemon() {
        let fx = fixture("faux/review");
        let wt = fx.spawn("fix", "quick", "change").await;
        let c = fx.commit(&wt, "src/a.rs");
        fx.done("T1-fix", &c, &["src/a.rs"]);
        let rv = fx.review_of("T1-fix").await;
        let s = scratch(&fx.sup.dir, &rv);
        // Detached like a daemon: no controlling terminal.
        let mut daemon = std::process::Command::new("setsid")
            .args(["sleep", "300"])
            .current_dir(s.join("work"))
            .spawn()
            .unwrap();
        assert!(fx.launcher.alive(rv.session.as_deref().unwrap()));
        fx.sup.retire_review(&rv.id);
        fx.until("scratch removed", |_| !s.exists()).await;
        let mut ended = None;
        for _ in 0..100 {
            ended = daemon.try_wait().unwrap();
            if ended.is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        if ended.is_none() {
            let _ = daemon.kill();
            let _ = daemon.wait();
        }
        assert!(ended.is_some(), "the daemon in the scratch folder was stopped");
    }

    #[tokio::test]
    async fn a_resent_done_on_the_approved_commit_is_ready_without_a_new_review() {
        let fx = fixture("faux/review");
        let wt = fx.spawn("fix", "quick", "change").await;
        let c = fx.commit(&wt, "src/a.rs");
        fx.done("T1-fix", &c, &["src/a.rs"]);
        let rv = fx.review_of("T1-fix").await;
        fx.report(&rv.id, json!({"verdict": "approved", "findings": []}));
        fx.until("ready", |fx| fx.row("T1-fix").state == State::Ready).await;
        assert_eq!(fx.row("T1-fix").approved_commit.as_deref(), Some(c.as_str()));

        // A failed merge, the user cleans up, super resends: the same commit again.
        fx.sup
            .edit(|b| {
                b.apply("T1-fix", Event::Merge)?;
                b.apply("T1-fix", Event::MergeReport(board::MergeResult::Failed("dirty".into())))
            })
            .unwrap()
            .unwrap();
        let send = json!({"action": "send", "task": "T1-fix", "message": "merge again"});
        fx.run(send.clone()).await.unwrap();
        assert_eq!(fx.row("T1-fix").state, State::Running);
        fx.done("T1-fix", &c, &["src/a.rs"]);
        fx.until("ready again", |fx| fx.row("T1-fix").state == State::Ready)
            .await;
        assert_eq!(fx.row("T1-fix").reviewer, None, "no new review");
        assert_eq!(fx.launcher.starts.lock().unwrap().len(), 2, "the task and one review");
        let said = fx.said();
        assert!(
            said.iter()
                .any(|l| l.starts_with("wake: T1-fix reported done (now ready)")
                    && l.contains("earlier approval of commit")
                    && l.contains(&c)),
            "{said:?}"
        );

        // Any other commit gets a review.
        fx.run(send).await.unwrap();
        let c2 = fx.commit(&wt, "src/b.rs");
        fx.done("T1-fix", &c2, &["src/a.rs", "src/b.rs"]);
        let rv2 = fx.review_of("T1-fix").await;
        assert_ne!(rv2.id, rv.id);
        assert_eq!(fx.row("T1-fix").state, State::Reviewing);
    }

    #[tokio::test]
    async fn a_follow_up_done_on_the_approved_commit_is_reviewed_again() {
        // L10.9 Y2: the approval judged the work before the follow-up, not the follow-up.
        let fx = fixture("faux/review");
        let wt = fx.spawn("fix", "quick", "change").await;
        let c = fx.commit(&wt, "src/a.rs");
        fx.done("T1-fix", &c, &["src/a.rs"]);
        let rv = fx.review_of("T1-fix").await;
        fx.report(&rv.id, json!({"verdict": "approved", "findings": []}));
        fx.until("ready", |fx| fx.row("T1-fix").state == State::Ready).await;
        let send = json!({"action": "send", "task": "T1-fix", "message": "also check the empty input"});
        fx.run(send).await.unwrap();
        fx.done("T1-fix", &c, &["src/a.rs"]);
        let rv2 = fx.review_of("T1-fix").await;
        assert_ne!(rv2.id, rv.id);
        assert_eq!(fx.row("T1-fix").state, State::Reviewing);
    }

    #[tokio::test]
    async fn a_branch_move_during_the_review_discards_its_verdict() {
        let fx = fixture("faux/review");
        let wt = fx.spawn("fix", "quick", "change").await;
        let c = fx.commit(&wt, "src/a.rs");
        fx.done("T1-fix", &c, &["src/a.rs"]);
        let rv = fx.review_of("T1-fix").await;
        // The user keeps working in the task's window.
        fx.commit(&wt, "src/b.rs");
        fx.links.kick();
        fx.until("running", |fx| fx.row("T1-fix").state == State::Running).await;
        assert_eq!(fx.row(&rv.id).state, State::Archived);
        fx.until("scratch removed", |fx| !scratch(&fx.sup.dir, &rv).exists())
            .await;
        assert_eq!(fx.row("T1-fix").review_rounds, 0);
        assert!(fx.said().iter().any(|l| l.contains("branch moved during its review")));
    }
}
