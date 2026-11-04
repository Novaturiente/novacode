//! A super task's side of the inbox (L10.9 "Reports and the inbox"): the
//! in-memory proof log, the `report` tool and the inbox writer.
//!
//! - [`ProofLog`] is an `after_tool` handler: it records every `bash` and
//!   `process` call (command, folder, exit code, last line) and every edit, and
//!   appends `[proof #N]` to each `bash` and `process` result so the model can
//!   cite `N` in `checks`. It lives only in this process's memory, so the
//!   task's own commands can't rewrite it; a restart starts it empty.
//! - [`ReportTool`] checks a report against the log and git, adds the fields
//!   code owns, and returns it in its result's details. It exists only in a
//!   task's own session (depth 0), never in its subagents.
//! - [`spawn_writer`] writes the inbox file once that result is in the session
//!   file, so a crash can't reuse a `seq`; [`rewrite_unapplied`] re-writes the
//!   last report at start when super hasn't applied it.

use super::board::{Board, Kind};
use futures_util::future::BoxFuture;
use novacode_core::hooks::{After, Handler, Run};
use novacode_core::session::{Entry, EntryKind, now_ms};
use novacode_tools::process::{Event as ProcEvent, ProcessManager};
use novacode_tools::{Tool, ToolCtx, ToolOutput};
use novacode_types::{AgentEvent, Message};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// The longest `summary` a report may carry.
pub const MAX_WORDS: usize = 80;
/// The longest verdict plus findings of a review report.
pub const MAX_VERDICT_CHARS: usize = 1000;
pub const MAX_FINDINGS: usize = 5;

/// The `ext` of the session entry that saves the task's identity.
const EXT: &str = "superagent";
const EXT_KIND: &str = "task";

// ---------------------------------------------------------------------------
// Identity

/// Which task this session is, from `NOVACODE_TASK`, `NOVACODE_TASK_TOKEN` and
/// `NOVACODE_SUPER_DIR`, or from the session entry an earlier start saved.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Identity {
    pub task: String,
    pub token: String,
    pub dir: PathBuf,
}

impl Identity {
    /// The three variables, when all are set and not empty.
    pub fn from_vars(var: impl Fn(&str) -> Option<String>) -> Option<Identity> {
        let [task, token, dir] = novacode_tools::env::TASK_VARS.map(|v| var(v).filter(|s| !s.is_empty()));
        Some(Identity {
            task: task?,
            token: token?,
            dir: dir?.into(),
        })
    }

    /// The identity saved on the session's branch (latest wins).
    pub fn saved<'a>(branch: impl IntoIterator<Item = &'a Entry>) -> Option<Identity> {
        branch
            .into_iter()
            .filter_map(|e| match &e.kind {
                EntryKind::ExtState { ext, kind, data } if ext == EXT && kind == EXT_KIND => {
                    serde_json::from_value(data.clone()).ok()
                }
                _ => None,
            })
            .last()
    }

    /// The session entry that saves it, so `novacode -r` brings `report` back.
    pub fn entry(&self) -> EntryKind {
        EntryKind::ExtState {
            ext: EXT.into(),
            kind: EXT_KIND.into(),
            data: serde_json::to_value(self).unwrap_or_default(),
        }
    }

    /// The identity for a session at agent `depth`: only depth 0 reports (a
    /// subagent never does), from the environment or else the saved entry.
    pub fn resolve(depth: u32, vars: Option<Identity>, saved: Option<Identity>) -> Option<Identity> {
        (depth == 0).then(|| vars.or(saved)).flatten()
    }
}

// ---------------------------------------------------------------------------
// Proof log

/// One `bash` or `process` call.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Check {
    pub n: u64,
    pub tool: String,
    pub command: String,
    pub folder: PathBuf,
    /// Unix ms.
    pub started_at: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<u64>,
    /// `None` once ended = killed (or no exit, as `process list`).
    pub exit_code: Option<i32>,
    pub last_line: String,
    /// Still running in the process manager (a `process` command, or a `bash`
    /// command moved to the background); its exit event completes it.
    pub running: bool,
    /// Set in a report: it started before the task's last edit (e.g. the red run
    /// of red-then-green), so it doesn't prove the final code.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub before_last_edit: bool,
    #[serde(skip)]
    process: Option<String>,
}

/// One edit tool call.
#[derive(Debug, Clone, PartialEq)]
pub struct Edit {
    pub path: PathBuf,
    /// Unix ms.
    pub at: u64,
}

#[derive(Default)]
struct Log {
    checks: Vec<Check>,
    edits: Vec<Edit>,
    /// Process id → (exit code, unix ms), from the process manager's exit events.
    ended: HashMap<String, (Option<i32>, u64)>,
}

/// The task's in-memory record of what it ran and edited.
#[derive(Default)]
pub struct ProofLog {
    log: Mutex<Log>,
}

/// The last line worth showing: not empty, not one of the tools' `[…]` status lines.
fn last_line(out: &ToolOutput) -> String {
    let text: String = out
        .content
        .iter()
        .filter_map(|b| match b {
            novacode_types::Block::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    let line = text
        .lines()
        .rev()
        .map(str::trim)
        .find(|l| !l.is_empty() && !l.starts_with('['))
        .unwrap_or_default();
    line.chars().take(200).collect()
}

impl ProofLog {
    /// Record a finished tool call; returns the proof number of a `bash` or `process` call.
    pub fn record(&self, tool: &str, args: &Value, out: &ToolOutput, run: &Run) -> Option<u64> {
        let now = now_ms();
        let mut log = self.log.lock().unwrap();
        match tool {
            "edit" | "write" | "apply_patch" if !out.is_error => {
                for p in &run.touched {
                    log.edits.push(Edit {
                        path: p.clone(),
                        at: now,
                    });
                }
                None
            }
            "bash" | "process" => {
                let s = |k: &str| args[k].as_str().unwrap_or_default();
                let (command, folder) = match (tool, s("action")) {
                    ("bash", _) => (s("command").to_string(), run.cwd.clone()),
                    (_, "start") => {
                        let cwd = args["cwd"].as_str().map_or(run.cwd.clone(), |c| run.cwd.join(c));
                        (s("command").to_string(), cwd)
                    }
                    (_, action) => (
                        format!("process {action} {}", s("id")).trim_end().to_string(),
                        run.cwd.clone(),
                    ),
                };
                let d = out.details.clone().unwrap_or_default();
                let process = d["process_id"].as_str().map(String::from);
                let exited = d.get("exit_code").map(|c| c.as_i64().map(|c| c as i32));
                let mut check = Check {
                    n: log.checks.len() as u64 + 1,
                    tool: tool.into(),
                    command,
                    folder,
                    started_at: now.saturating_sub(run.duration_ms),
                    ended_at: None,
                    exit_code: None,
                    last_line: last_line(out),
                    running: false,
                    before_last_edit: false,
                    process: process.clone(),
                };
                match (exited, process.and_then(|p| log.ended.get(&p).copied())) {
                    (Some(code), _) => (check.exit_code, check.ended_at) = (code, Some(now)),
                    (None, Some((code, at))) => (check.exit_code, check.ended_at) = (code, Some(at)),
                    (None, None) if check.process.is_some() => check.running = true,
                    (None, None) => check.ended_at = Some(now),
                }
                // Files a `bash` call changed are edits stamped with its own
                // start: stale for every earlier check, not for this one.
                if tool == "bash" {
                    let at = check.started_at;
                    let edits = run.touched.iter().map(|p| Edit { path: p.clone(), at });
                    log.edits.extend(edits);
                }
                let n = check.n;
                log.checks.push(check);
                Some(n)
            }
            _ => None,
        }
    }

    /// A process exited: complete every check still waiting on it.
    pub fn ended(&self, id: &str, code: Option<i32>, at: u64) {
        let mut log = self.log.lock().unwrap();
        log.ended.insert(id.to_string(), (code, at));
        for c in log
            .checks
            .iter_mut()
            .filter(|c| c.running && c.process.as_deref() == Some(id))
        {
            c.running = false;
            c.exit_code = code;
            c.ended_at = Some(at);
        }
    }

    /// Complete checks from the process manager's exit events.
    pub fn watch(self: &Arc<Self>, pm: &Arc<ProcessManager>) {
        let mut events = pm.events.subscribe();
        let log = Arc::downgrade(self);
        tokio::spawn(async move {
            loop {
                match events.recv().await {
                    Ok(ProcEvent::Ended { id, end, .. }) => match log.upgrade() {
                        Some(log) => log.ended(&id, end.code, end.at_ms),
                        None => break,
                    },
                    Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                    Err(_) => break,
                }
            }
        });
    }

    /// Pick up exits a lagged watcher missed, from the process manager's list.
    fn sync(&self, pm: &ProcessManager) {
        for i in pm.list() {
            if let Some(end) = i.end {
                self.ended(&i.id, end.code, end.at_ms);
            }
        }
    }

    pub fn check(&self, n: u64) -> Option<Check> {
        self.log.lock().unwrap().checks.iter().find(|c| c.n == n).cloned()
    }

    pub fn last_edit(&self) -> Option<u64> {
        self.log.lock().unwrap().edits.iter().map(|e| e.at).max()
    }

    pub fn edited(&self) -> Vec<PathBuf> {
        let mut v: Vec<PathBuf> = self.log.lock().unwrap().edits.iter().map(|e| e.path.clone()).collect();
        v.sort();
        v.dedup();
        v
    }
}

impl Handler for ProofLog {
    /// No `[tag]`: the mark is appended as `[proof #N]`.
    fn label(&self) -> &str {
        ""
    }
    fn after_tool<'a>(
        &'a self,
        tool: &'a str,
        args: &'a Value,
        out: &'a ToolOutput,
        run: &'a Run,
    ) -> BoxFuture<'a, Option<After>> {
        let n = self.record(tool, args, out, run);
        Box::pin(async move { n.map(|n| After::append(format!("[proof #{n}]"))) })
    }
    /// Files a `bash` call changed count as edits.
    fn wants_touched(&self) -> bool {
        true
    }
}

// ---------------------------------------------------------------------------
// The report tool

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum WorkState {
    Progress,
    Done,
    Blocked,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum Verdict {
    Approved,
    NeedsProof,
    Rejected,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum Merge {
    Merged,
    PrOpened,
    Failed,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Args {
    /// Work task: progress | done | blocked | failed.
    state: Option<WorkState>,
    /// Review task, instead of `state`: approved | needs-proof | rejected.
    verdict: Option<Verdict>,
    /// Merging task, instead of `state`: merged | pr-opened | failed.
    merge: Option<Merge>,
    /// At most 80 words; for `merge`, the one-line reason.
    summary: Option<String>,
    /// Only with `state: blocked`: what you need answered.
    question: Option<String>,
    /// Proof numbers from the `[proof #N]` marks (e.g. "#4"), plus files a check produced and URLs.
    #[serde(default)]
    checks: Vec<String>,
    /// Review task: at most 5 findings or proof requests (with the verdict, 1000 characters at most).
    #[serde(default)]
    findings: Vec<String>,
    /// Review task: base-commit check results, outside the 1000-character cap.
    baseline: Option<Value>,
}

/// What `checks` holds besides proof numbers, for the field and its errors.
const ARTIFACTS_ONLY: &str = "Besides `#N` proof numbers, `checks` holds only files a check produced \
(screenshots, logs) and URLs, never source files, commands or results: cite the `#N` of the check that ran \
them, and put text in `summary`.";

/// One `checks` entry.
enum Cited {
    Proof(u64),
    Url(String),
    Artifact(String),
}

fn cite(s: &str) -> Cited {
    let t = s.trim();
    // `[proof #N]`, as the mark shows on the tool result.
    let num = t.strip_prefix('[').and_then(|r| r.strip_suffix(']')).unwrap_or(t);
    let num = num.strip_prefix("proof").map(str::trim_start).unwrap_or(num);
    let num = num.strip_prefix('#').unwrap_or(num);
    if let Ok(n) = num.parse() {
        return Cited::Proof(n);
    }
    if t.starts_with("http://") || t.starts_with("https://") {
        return Cited::Url(t.into());
    }
    Cited::Artifact(t.into())
}

/// What git says about the task's folder.
#[derive(Default)]
struct Git {
    repo: bool,
    branch: Option<String>,
    commit: Option<String>,
    /// Uncommitted files, copies from `worktree.copy` not counted.
    dirty: usize,
    linked: bool,
    files: Vec<String>,
}

fn git_out(dir: &Path, args: &[&str]) -> Option<String> {
    let o = novacode_base::git::command_in(dir)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()
        .filter(|o| o.status.success())?;
    Some(String::from_utf8_lossy(&o.stdout).trim().to_string())
}

fn git_facts(cwd: &Path, base: Option<&str>) -> Git {
    if git_out(cwd, &["rev-parse", "--show-toplevel"]).is_none() {
        return Git::default();
    }
    let copies = novacode_tools::worktree::copies(cwd);
    let (dirty, linked) = match novacode_tools::worktree::here(cwd) {
        Ok(h) => (h.dirty, true),
        Err(_) => {
            let status = git_out(cwd, &["status", "--porcelain", "-uall", "--ignore-submodules=all"]);
            (status.unwrap_or_default().lines().count(), false)
        }
    };
    let files = base
        .and_then(|b| git_out(cwd, &["diff", "--name-only", &format!("{b}...HEAD")]))
        .unwrap_or_default()
        .lines()
        .filter(|f| !f.is_empty() && !copies.iter().any(|c| f.starts_with(c.as_str())))
        .map(String::from)
        .collect();
    Git {
        repo: true,
        branch: git_out(cwd, &["branch", "--show-current"]).filter(|b| !b.is_empty()),
        commit: git_out(cwd, &["rev-parse", "HEAD"]),
        dirty,
        linked,
        files,
    }
}

/// `report`: a task's only way to tell super how it went.
pub struct ReportTool {
    pub id: Identity,
    pub proof: Arc<ProofLog>,
    /// The next report's `seq`.
    seq: AtomicU64,
}

impl ReportTool {
    /// `last_seq`: the highest `seq` already on the session's branch ([`last_seq`]).
    pub fn new(id: Identity, proof: Arc<ProofLog>, last_seq: u64) -> Self {
        ReportTool {
            id,
            proof,
            seq: AtomicU64::new(last_seq + 1),
        }
    }

    /// The report, or why it is refused.
    async fn build(&self, a: Args, ctx: &ToolCtx) -> Result<Value, String> {
        let row = Board::load(&self.id.dir)
            .ok()
            .and_then(|b| b.task(&self.id.task).cloned())
            .unwrap_or_default();
        let review = row.kind == Kind::Review;
        let forms = [a.state.is_some(), a.verdict.is_some(), a.merge.is_some()];
        if forms.iter().filter(|f| **f).count() != 1 {
            return Err("give exactly one of `state` (work), `verdict` (review) or `merge` (merging)".into());
        }
        if review && a.verdict.is_none() {
            return Err("you are a review task: report with `verdict` and `findings`".into());
        }
        if !review && a.verdict.is_some() {
            return Err("`verdict` is for review tasks; report with `state`".into());
        }
        let summary = a.summary.clone().unwrap_or_default();
        if a.verdict.is_none() && summary.trim().is_empty() {
            return Err("`summary` is required".into());
        }
        let words = summary.split_whitespace().count();
        if words > MAX_WORDS {
            return Err(format!(
                "`summary` has {words} words; at most {MAX_WORDS}. Shorten it and report again"
            ));
        }
        if a.merge.is_some() && summary.trim().contains('\n') {
            return Err("a `merge` report's reason is one line".into());
        }
        if a.question.is_some() && a.state != Some(WorkState::Blocked) {
            return Err("`question` is only allowed with `state: blocked`".into());
        }
        if a.verdict.is_some() {
            if a.findings.len() > MAX_FINDINGS {
                return Err(format!("at most {MAX_FINDINGS} findings; merge or drop some"));
            }
            let verdict = serde_json::to_value(a.verdict).unwrap_or_default();
            let chars = verdict.as_str().unwrap_or_default().chars().count()
                + a.findings.iter().map(|f| f.chars().count()).sum::<usize>();
            if chars > MAX_VERDICT_CHARS {
                return Err(format!(
                    "verdict and findings are {chars} characters; at most {MAX_VERDICT_CHARS}"
                ));
            }
        }
        if let Some(pm) = &ctx.processes {
            self.proof.sync(pm);
        }
        let base = row.base_commit.clone().or(row.base_branch.clone());
        let cwd = ctx.cwd.clone();
        let git = tokio::task::spawn_blocking(move || git_facts(&cwd, base.as_deref()))
            .await
            .unwrap_or_default();
        let write = row.write || git.linked;
        let done = a.state == Some(WorkState::Done);
        let last_edit = self.proof.last_edit();

        let (mut checks, mut artifacts) = (Vec::new(), Vec::new());
        for c in &a.checks {
            match cite(c) {
                Cited::Proof(n) => {
                    let Some(check) = self.proof.check(n) else {
                        return Err(format!(
                            "no proof #{n}: cite the `[proof #N]` marks on bash and process results of this session"
                        ));
                    };
                    if done && check.running {
                        return Err(format!(
                            "proof #{n} (`{}`) is still running; wait for it to exit, then report",
                            check.command
                        ));
                    }
                    let mut check = check;
                    check.before_last_edit = last_edit.is_some_and(|e| check.started_at < e);
                    checks.push(check);
                }
                Cited::Url(u) => artifacts.push(u),
                Cited::Artifact(p) => {
                    let path = ctx.path(&p);
                    if done {
                        let mtime = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
                        let Some(mtime) = mtime else {
                            return Err(format!("{p} doesn't exist. {ARTIFACTS_ONLY}"));
                        };
                        let ms = mtime
                            .duration_since(std::time::UNIX_EPOCH)
                            .map_or(0, |d| d.as_millis() as u64);
                        if last_edit.is_some_and(|e| ms < e) {
                            return Err(format!(
                                "{p} is older than your last edit: a file a check produced must be made after it. \
                                 {ARTIFACTS_ONLY}"
                            ));
                        }
                    }
                    artifacts.push(path.display().to_string());
                }
            }
        }
        if done && write && checks.is_empty() {
            return Err(
                "`done` needs proof: `checks` is a list field of proof numbers, like [\"#7\"]: cite there (not in \
                 `summary`) the `[proof #N]` numbers of the checks you ran after your last edit"
                    .into(),
            );
        }
        if done && write && checks.iter().all(|c| c.before_last_edit) {
            let cited: Vec<String> = checks.iter().map(|c| format!("#{} (`{}`)", c.n, c.command)).collect();
            return Err(format!(
                "every cited check ran before your last edit ({}): run a check now and cite its new number too; \
                 checks from before the edit may stay, marked as such",
                cited.join(", ")
            ));
        }
        if done && write && git.repo && git.dirty > 0 {
            return Err(format!(
                "{} uncommitted file(s): commit your work, then report `done`",
                git.dirty
            ));
        }
        let files: Vec<String> = if git.repo {
            git.files
        } else {
            self.proof.edited().iter().map(|p| p.display().to_string()).collect()
        };
        let seq = self.seq.fetch_add(1, Ordering::Relaxed);
        let mut r = json!({
            "id": novacode_base::new_id().to_string(),
            "task": self.id.task,
            "seq": seq,
            "token": self.id.token,
            "kind": row.kind,
            "write": write,
            "files_changed": files,
            "checks": checks,
            "artifacts": artifacts,
            "at": now_ms(),
        });
        let mut put = |k: &str, v: Value| {
            if !v.is_null() {
                r[k] = v;
            }
        };
        put("branch", json!(git.branch));
        put("commit", json!(git.commit));
        put("state", json!(a.state));
        put("verdict", json!(a.verdict));
        put("merge", json!(a.merge));
        put("summary", json!(a.summary.filter(|s| !s.trim().is_empty())));
        put("question", json!(a.question));
        if a.verdict.is_some() {
            put("findings", json!(a.findings));
            put("baseline", a.baseline.unwrap_or(Value::Null));
        }
        Ok(r)
    }
}

impl Tool for ReportTool {
    fn name(&self) -> &str {
        "report"
    }
    fn description(&self) -> &str {
        "Report to super, the session that started this task. Work task: `state` (progress | done | blocked | failed), `summary` (at most 80 words), `question` (only with blocked), `checks` (a list of the `[proof #N]` numbers of the bash and process calls that prove it, like [\"#7\"], plus files a check produced, such as screenshots or logs, and URLs; `done` on a write task needs one check run after your last edit, and earlier ones are marked as before the last edit). Review task: `verdict` and `findings`. Merging: `merge` and a one-line `summary`."
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "state": { "type": "string", "enum": ["progress", "done", "blocked", "failed"], "description": "Work task: how it went." },
                "verdict": { "type": "string", "enum": ["approved", "needs-proof", "rejected"], "description": "Review task, instead of `state`." },
                "merge": { "type": "string", "enum": ["merged", "pr-opened", "failed"], "description": "Merging task, instead of `state`." },
                "summary": { "type": "string", "description": "At most 80 words; for `merge`, the one-line reason." },
                "question": { "type": "string", "description": "Only with `state: blocked`: what you need answered." },
                "checks": { "type": "array", "items": { "type": "string" }, "description": "Proof numbers from the `[proof #N]` marks on your bash and process results (e.g. \"#4\"); `done` on a write task needs at least one check that ran after your last edit and has exited, and a check from before it (such as the failing run of red-then-green) is allowed but marked `before the last edit`. Plus files a check produced (screenshots, logs) and URLs; never source files, commands or results: those go in `summary`." },
                "findings": { "type": "array", "items": { "type": "string" }, "description": "Review task: at most 5 findings or proof requests (with the verdict, 1000 characters at most)." },
                "baseline": { "type": "object", "description": "Review task: base-commit check results, outside the 1000-character cap: {\"checks\": [{\"folder\", \"command\", \"exit_codes\": [first run, re-run], \"failing\": [test names]}]}." }
            }
        })
    }
    fn guidelines(&self) -> &[&str] {
        &[
            "Call `report` when you are done, blocked or failed. `done` needs `checks`: the `[proof #N]` numbers of finished checks, at least one run after your last edit (earlier ones, like a red test run, are allowed and marked), and committed work in a git worktree.",
            "The proof log lives in memory: after a restart or resume it is empty, so run your checks again before citing them.",
        ]
    }
    fn large_output(&self) -> bool {
        false
    }
    fn sequential(&self) -> bool {
        true
    }
    fn execute<'a>(&'a self, args: Value, ctx: &'a ToolCtx) -> BoxFuture<'a, ToolOutput> {
        Box::pin(async move {
            let a: Args = match novacode_tools::parse(args) {
                Ok(a) => a,
                Err(e) => return e,
            };
            match self.build(a, ctx).await {
                Ok(r) => {
                    let cited: Vec<String> = r["checks"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .map(|c| {
                            let s = match c["exit_code"].as_i64() {
                                Some(code) => format!("#{} exit {code}", c["n"]),
                                None => format!("#{} killed", c["n"]),
                            };
                            if c["before_last_edit"] == true {
                                s + " (before the last edit)"
                            } else {
                                s
                            }
                        })
                        .collect();
                    let mut text = format!("Report {} sent to super.", r["seq"]);
                    if !cited.is_empty() {
                        text += &format!(" Checks: {}.", cited.join(", "));
                    }
                    ToolOutput {
                        details: Some(json!({ "report": r })),
                        ..ToolOutput::text(text)
                    }
                }
                Err(why) => ToolOutput::error(format!("Report not sent: {why}.")),
            }
        })
    }
}

// ---------------------------------------------------------------------------
// The inbox writer

/// The report a `report` result carries, when it was sent.
fn sent(m: &Message) -> Option<&Value> {
    match m {
        Message::ToolResult {
            tool_name,
            is_error: false,
            details: Some(d),
            ..
        } if tool_name == "report" => d.get("report").filter(|r| r.is_object()),
        _ => None,
    }
}

fn reports<'a>(branch: impl IntoIterator<Item = &'a Entry>) -> impl Iterator<Item = &'a Value> {
    branch.into_iter().filter_map(|e| match &e.kind {
        EntryKind::Message { message } => sent(message),
        _ => None,
    })
}

/// The highest `seq` sent on this branch (0 = none).
pub fn last_seq<'a>(branch: impl IntoIterator<Item = &'a Entry>) -> u64 {
    reports(branch).filter_map(|r| r["seq"].as_u64()).max().unwrap_or(0)
}

/// `<dir>/inbox/<task>-<seq>.json`, written through a temporary file and a rename.
pub fn write_inbox(dir: &Path, report: &Value) -> std::io::Result<PathBuf> {
    let (task, seq) = (
        report["task"].as_str().unwrap_or_default(),
        report["seq"].as_u64().unwrap_or(0),
    );
    let inbox = dir.join("inbox");
    std::fs::create_dir_all(&inbox)?;
    let path = inbox.join(format!("{task}-{seq}.json"));
    let bytes = serde_json::to_vec(report).map_err(std::io::Error::other)?;
    novacode_base::storage::write_atomic_no_backup(&path, &bytes, novacode_base::storage::Durability::Durable)?;
    Ok(path)
}

/// Write each sent report to the inbox. The session's message event comes after
/// the entry is appended, so the entry is always on disk before the file.
pub fn spawn_writer(mut events: tokio::sync::broadcast::Receiver<AgentEvent>, dir: PathBuf) {
    tokio::spawn(async move {
        loop {
            match events.recv().await {
                Ok(AgentEvent::Message(m)) => {
                    if let Some(r) = sent(&m)
                        && let Err(e) = write_inbox(&dir, r)
                    {
                        tracing::warn!("super inbox: report not written: {e}");
                    }
                }
                Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                Err(_) => break,
            }
        }
    });
}

/// At start or resume: re-write the branch's last report when super's board
/// doesn't list it as applied (a crash between the entry and the file).
pub fn rewrite_unapplied<'a>(branch: impl IntoIterator<Item = &'a Entry>, id: &Identity) -> Option<PathBuf> {
    let last = reports(branch).last()?;
    let board = Board::load(&id.dir).ok()?;
    let task = board.task(&id.task)?;
    let rid = last["id"].as_str()?;
    if task.applied.iter().any(|a| a == rid) {
        return None;
    }
    write_inbox(&id.dir, last).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::superagent::board::Task;
    use std::process::Command;
    use std::time::Duration;

    fn text(o: &ToolOutput) -> String {
        o.content
            .iter()
            .filter_map(|b| match b {
                novacode_types::Block::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn ctx(dir: &Path) -> ToolCtx {
        ToolCtx {
            cwd: dir.to_path_buf(),
            env: Arc::new(novacode_tools::LocalEnv),
            cancel: Default::default(),
            updates: None,
            files: Default::default(),
            session_id: "s".into(),
            processes: None,
            native_tools: false,
            nested: None,
            script: false,
        }
    }

    fn git(dir: &Path, args: &[&str]) {
        let ok = Command::new("git")
            .args(["-c", "user.email=t@t", "-c", "user.name=t"])
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap()
            .status
            .success();
        assert!(ok, "git {args:?}");
    }

    /// A task folder (`work`, a git repo when `repo`) and super's folder with
    /// a board row for task `T1`.
    struct Fixture {
        _tmp: tempfile::TempDir,
        work: PathBuf,
        tool: ReportTool,
        hooks: novacode_core::hooks::Hooks,
        ctx: ToolCtx,
    }

    impl Fixture {
        fn new(repo: bool, row: Task) -> Fixture {
            let tmp = tempfile::tempdir().unwrap();
            let work = tmp.path().join("work");
            let dir = tmp.path().join("super");
            std::fs::create_dir_all(&work).unwrap();
            std::fs::create_dir_all(&dir).unwrap();
            if repo {
                git(&work, &["init", "-q", "-b", "main"]);
                std::fs::write(work.join("a.txt"), "1").unwrap();
                git(&work, &["add", "."]);
                git(&work, &["commit", "-qm", "base"]);
            }
            let board = Board {
                tasks: vec![Task {
                    id: "T1".into(),
                    token: "tok".into(),
                    ..row
                }],
                ..Board::default()
            };
            board.save(&dir).unwrap();
            let proof = Arc::new(ProofLog::default());
            let id = Identity {
                task: "T1".into(),
                token: "tok".into(),
                dir,
            };
            let mut ctx = ctx(&work);
            ctx.processes = Some(ProcessManager::new(&tmp.path().join("procs"), "s"));
            Fixture {
                hooks: novacode_core::hooks::Hooks {
                    handlers: vec![proof.clone()],
                },
                tool: ReportTool::new(id, proof, 0),
                ctx,
                work,
                _tmp: tmp,
            }
        }

        /// Run a tool through the hooks, as the agent does.
        async fn call(&self, tool: &dyn Tool, args: Value) -> ToolOutput {
            let started = self.hooks.start(tool.name(), &self.ctx.cwd).await;
            let mut out = tool.execute(args.clone(), &self.ctx).await;
            self.hooks
                .after_tool("c", tool.name(), &args, &self.ctx.cwd, started, &mut out)
                .await;
            out
        }

        async fn bash(&self, cmd: &str) -> ToolOutput {
            self.call(&novacode_tools::bash::Bash, json!({ "command": cmd })).await
        }

        async fn edit(&self, file: &str) {
            let out = self
                .call(&novacode_tools::write::Write, json!({"path": file, "content": "x"}))
                .await;
            assert!(!out.is_error, "{}", text(&out));
        }

        async fn report(&self, args: Value) -> ToolOutput {
            self.call(&self.tool, args).await
        }
    }

    fn write_row() -> Task {
        Task {
            write: true,
            ..Task::default()
        }
    }

    fn refused(o: &ToolOutput, why: &str) {
        assert!(o.is_error && text(o).contains(why), "want `{why}`: {}", text(o));
    }

    fn report(o: &ToolOutput) -> Value {
        assert!(!o.is_error, "{}", text(o));
        o.details.as_ref().unwrap()["report"].clone()
    }

    #[tokio::test]
    async fn bash_and_process_results_carry_proof_marks() {
        let f = Fixture::new(false, Task::default());
        let o = f.bash("echo one").await;
        assert_eq!(text(&o), "one\n[proof #1]");
        let o = f.bash("exit 3").await;
        assert!(text(&o).ends_with("[exit code 3]\n[proof #2]"), "{}", text(&o));
        let o = f
            .call(
                &novacode_tools::process::ProcessTool,
                json!({"action": "start", "command": "echo up; exit 4"}),
            )
            .await;
        assert!(text(&o).ends_with("[proof #3]"), "{}", text(&o));
        let o = f
            .call(&novacode_tools::process::ProcessTool, json!({"action": "list"}))
            .await;
        assert!(text(&o).ends_with("[proof #4]"), "{}", text(&o));
        f.edit("b.txt").await;
        assert_eq!(f.tool.proof.check(4).unwrap().command, "process list");
        assert_eq!(f.tool.proof.edited(), vec![f.work.join("b.txt")]);
        assert_eq!(f.tool.proof.check(5), None, "edits get no number");
    }

    #[tokio::test]
    async fn report_records_real_commands_and_exit_codes() {
        let f = Fixture::new(false, write_row());
        f.edit("b.txt").await;
        f.bash("echo ok; echo last line").await;
        f.bash("exit 2").await;
        let r = report(
            &f.report(json!({"state": "done", "summary": "built it", "checks": ["#1", "proof #2"]}))
                .await,
        );
        let checks = r["checks"].as_array().unwrap();
        assert_eq!(checks.len(), 2);
        assert_eq!(checks[0]["command"], "echo ok; echo last line");
        assert_eq!(checks[0]["exit_code"], 0);
        assert_eq!(checks[0]["last_line"], "last line");
        assert_eq!(checks[0]["folder"], json!(f.work));
        assert_eq!(checks[1]["exit_code"], 2);
        assert_eq!(r["task"], "T1");
        assert_eq!(r["token"], "tok");
        assert_eq!(r["seq"], 1);
        assert_eq!(r["state"], "done");
        assert_eq!(r["write"], true);
        assert_eq!(r["kind"], "work");
        assert_eq!(
            r["files_changed"],
            json!([f.work.join("b.txt")]),
            "non-git: from the proof log"
        );
        assert!(r.get("commit").is_none(), "non-git `done` needs no commit");
        assert!(r["id"].as_str().is_some_and(|i| !i.is_empty()));
        let next = report(&f.report(json!({"state": "progress", "summary": "more"})).await);
        assert_eq!(next["seq"], 2);
        assert_ne!(next["id"], r["id"]);
    }

    #[tokio::test]
    async fn summary_over_80_words_is_refused() {
        let f = Fixture::new(false, Task::default());
        let long = vec!["word"; 81].join(" ");
        refused(
            &f.report(json!({"state": "progress", "summary": long})).await,
            "81 words",
        );
        let ok = vec!["word"; 80].join(" ");
        report(&f.report(json!({"state": "progress", "summary": ok})).await);
    }

    #[tokio::test]
    async fn question_only_on_blocked() {
        let f = Fixture::new(false, Task::default());
        for state in ["progress", "done", "failed"] {
            let o = f
                .report(json!({"state": state, "summary": "s", "question": "which?"}))
                .await;
            refused(&o, "only allowed with `state: blocked`");
        }
        let r = report(
            &f.report(json!({"state": "blocked", "summary": "s", "question": "which?"}))
                .await,
        );
        assert_eq!(r["question"], "which?");
    }

    #[tokio::test]
    async fn write_task_done_without_checks_is_refused() {
        let f = Fixture::new(false, write_row());
        refused(
            &f.report(json!({"state": "done", "summary": "s"})).await,
            "`done` needs proof",
        );
        refused(
            &f.report(json!({"state": "done", "summary": "s", "checks": ["shot.png"]}))
                .await,
            "doesn't exist",
        );
        refused(
            &f.report(json!({"state": "done", "summary": "s", "checks": ["#9"]}))
                .await,
            "no proof #9",
        );
        // A read task's answer needs no checks.
        let read = Fixture::new(false, Task::default());
        report(&read.report(json!({"state": "done", "summary": "the answer"})).await);
    }

    #[tokio::test]
    async fn only_checks_before_the_last_edit_are_refused() {
        let f = Fixture::new(false, write_row());
        f.bash("echo test").await;
        tokio::time::sleep(Duration::from_millis(5)).await;
        f.edit("b.txt").await;
        refused(
            &f.report(json!({"state": "done", "summary": "s", "checks": ["#1"]}))
                .await,
            "every cited check ran before your last edit",
        );
        f.bash("echo test").await;
        let r = report(
            &f.report(json!({"state": "done", "summary": "s", "checks": ["#2"]}))
                .await,
        );
        assert!(r["checks"][0].get("before_last_edit").is_none());
    }

    #[tokio::test]
    async fn check_before_the_last_edit_is_marked_beside_a_fresh_one() {
        // Red then green: the failing run predates the fix.
        let f = Fixture::new(false, write_row());
        f.bash("exit 1").await;
        tokio::time::sleep(Duration::from_millis(5)).await;
        f.edit("b.txt").await;
        f.bash("echo green").await;
        let o = f
            .report(json!({"state": "done", "summary": "s", "checks": ["#1", "#2"]}))
            .await;
        let r = report(&o);
        assert_eq!(r["checks"][0]["before_last_edit"], true);
        assert!(r["checks"][1].get("before_last_edit").is_none());
        assert!(text(&o).contains("#1 exit 1 (before the last edit)"), "{}", text(&o));
    }

    #[tokio::test]
    async fn done_without_checks_names_the_field_shape() {
        let f = Fixture::new(false, write_row());
        let o = f
            .report(json!({"state": "done", "summary": "tests pass, proof #3"}))
            .await;
        refused(&o, "`checks` is a list field of proof numbers, like [\"#7\"]");
    }

    #[tokio::test]
    async fn bash_edit_after_a_check_makes_it_stale() {
        let f = Fixture::new(true, write_row());
        f.bash("echo test").await;
        tokio::time::sleep(Duration::from_millis(5)).await;
        f.bash("echo more >> a.txt").await;
        refused(
            &f.report(json!({"state": "done", "summary": "s", "checks": ["#1"]}))
                .await,
            "every cited check ran before your last edit",
        );
    }

    #[tokio::test]
    async fn bash_check_that_writes_a_file_can_be_cited() {
        // A read row: no commit needed, so only staleness can refuse.
        let f = Fixture::new(true, Task::default());
        f.bash("echo more >> a.txt").await;
        assert_eq!(f.tool.proof.edited(), vec![f.work.join("a.txt")]);
        report(
            &f.report(json!({"state": "done", "summary": "s", "checks": ["#1"]}))
                .await,
        );
    }

    #[tokio::test]
    async fn missing_or_stale_artifact_is_refused() {
        let f = Fixture::new(false, write_row());
        std::fs::write(f.work.join("old.png"), "png").unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
        f.edit("b.txt").await;
        f.bash("echo test").await;
        let done = |a: &str| json!({"state": "done", "summary": "s", "checks": ["#1", a]});
        refused(&f.report(done("old.png")).await, "older than your last edit");
        refused(&f.report(done("old.png")).await, "never source files");
        refused(&f.report(done("none.png")).await, "doesn't exist");
        // Text and commands in `checks`: the error says where they go.
        let o = f.report(done("python3 -m unittest: OK (2 tests)")).await;
        refused(&o, "only files a check produced");
        refused(&o, "`summary`");
        // The mark as the tool result shows it is a proof number.
        let r = report(&f.report(done("[proof #1]")).await);
        assert_eq!(r["checks"].as_array().unwrap().len(), 2);
        let schema = f.tool.schema().to_string();
        assert!(schema.contains("never source files, commands or results"), "{schema}");
        std::fs::write(f.work.join("new.png"), "png").unwrap();
        let r = report(&f.report(done("new.png")).await);
        assert_eq!(r["artifacts"], json!([f.work.join("new.png")]));
        let r = report(&f.report(done("https://example.com/shot")).await);
        assert_eq!(r["artifacts"], json!(["https://example.com/shot"]));
    }

    #[tokio::test]
    async fn still_running_checks_are_refused_until_they_exit() {
        let f = Fixture::new(false, write_row());
        f.edit("b.txt").await;
        // A bash command moved to the background, and a process command.
        let o = f
            .call(
                &novacode_tools::bash::Bash,
                json!({"command": "sleep 4; exit 5", "timeout": 1}),
            )
            .await;
        assert!(text(&o).contains("moved to the process manager"), "{}", text(&o));
        f.call(
            &novacode_tools::process::ProcessTool,
            json!({"action": "start", "command": "sleep 4; echo done"}),
        )
        .await;
        f.tool.proof.watch(f.ctx.processes.as_ref().unwrap());
        for n in ["#1", "#2"] {
            let o = f.report(json!({"state": "done", "summary": "s", "checks": [n]})).await;
            refused(&o, "still running");
        }
        // A progress report may cite a running check.
        report(
            &f.report(json!({"state": "progress", "summary": "s", "checks": ["#1"]}))
                .await,
        );
        tokio::time::sleep(Duration::from_millis(3800)).await;
        let r = report(
            &f.report(json!({"state": "done", "summary": "s", "checks": ["#1", "#2"]}))
                .await,
        );
        assert_eq!(r["checks"][0]["exit_code"], 5);
        assert_eq!(r["checks"][1]["exit_code"], 0);
        assert_eq!(r["checks"][1]["running"], false);
    }

    #[tokio::test]
    async fn uncommitted_changes_refuse_done_in_git() {
        let f = Fixture::new(
            true,
            Task {
                base_branch: Some("main".into()),
                ..write_row()
            },
        );
        f.edit("b.txt").await;
        f.bash("echo test").await;
        let done = json!({"state": "done", "summary": "s", "checks": ["#1"]});
        refused(&f.report(done.clone()).await, "1 uncommitted file(s)");
        git(&f.work, &["checkout", "-q", "-b", "feature"]);
        git(&f.work, &["add", "."]);
        git(&f.work, &["commit", "-qm", "work"]);
        let r = report(&f.report(done).await);
        assert_eq!(r["branch"], "feature");
        assert_eq!(r["files_changed"], json!(["b.txt"]));
        let head = String::from_utf8(
            Command::new("git")
                .args(["rev-parse", "HEAD"])
                .current_dir(&f.work)
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap();
        assert_eq!(r["commit"], head.trim());
    }

    #[tokio::test]
    async fn review_and_merge_forms_skip_write_gates() {
        let review = Fixture::new(
            true,
            Task {
                kind: Kind::Review,
                ..Task::default()
            },
        );
        std::fs::write(review.work.join("scratch.txt"), "x").unwrap();
        refused(
            &review.report(json!({"state": "done", "summary": "s"})).await,
            "you are a review task",
        );
        let r = report(
            &review
                .report(json!({"verdict": "needs-proof", "findings": ["run the tests"], "baseline": {"cargo test": 0}}))
                .await,
        );
        assert_eq!(
            (r["verdict"].clone(), r["kind"].clone()),
            (json!("needs-proof"), json!("review"))
        );
        assert_eq!(r["baseline"], json!({"cargo test": 0}));
        let six = vec!["f"; 6];
        refused(
            &review.report(json!({"verdict": "rejected", "findings": six})).await,
            "at most 5",
        );
        let long = vec!["x".repeat(400); 3];
        refused(
            &review.report(json!({"verdict": "rejected", "findings": long})).await,
            "at most 1000",
        );

        // A merging write task with uncommitted changes and no checks.
        let work = Fixture::new(true, write_row());
        std::fs::write(work.work.join("dirty.txt"), "x").unwrap();
        for m in ["merged", "pr-opened", "failed"] {
            let r = report(&work.report(json!({"merge": m, "summary": "rebased and merged"})).await);
            assert_eq!(r["merge"], m);
        }
        refused(
            &work.report(json!({"merge": "failed", "summary": "a\nb"})).await,
            "one line",
        );
        refused(&work.report(json!({"verdict": "approved"})).await, "for review tasks");
        refused(
            &work
                .report(json!({"state": "done", "merge": "merged", "summary": "s"}))
                .await,
            "exactly one",
        );
    }

    fn entry(kind: EntryKind) -> Entry {
        Entry {
            id: novacode_base::new_id().to_string(),
            parent: None,
            ts_ms: 0,
            kind,
        }
    }

    fn result_entry(o: &ToolOutput) -> Entry {
        entry(EntryKind::Message {
            message: Message::ToolResult {
                call_id: "c".into(),
                tool_name: "report".into(),
                content: o.content.clone(),
                is_error: o.is_error,
                details: o.details.clone(),
            },
        })
    }

    #[tokio::test]
    async fn inbox_file_is_written_after_the_session_entry() {
        let f = Fixture::new(false, Task::default());
        let o = f.report(json!({"state": "progress", "summary": "half way"})).await;
        let dir = f.tool.id.dir.clone();
        let file = dir.join("inbox/T1-1.json");
        assert!(!file.exists(), "the tool itself writes no file");
        let (tx, rx) = tokio::sync::broadcast::channel(16);
        spawn_writer(rx, dir.clone());
        // The agent appends the result entry, then sends this event.
        let Entry {
            kind: EntryKind::Message { message },
            ..
        } = result_entry(&o)
        else {
            unreachable!()
        };
        tx.send(AgentEvent::Message(message)).unwrap();
        for _ in 0..100 {
            if file.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let written: crate::superagent::inbox::Report = serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
        assert_eq!(
            (written.task.as_str(), written.seq, written.token.as_str()),
            ("T1", 1, "tok")
        );
        let board = Board::load(&dir).unwrap();
        let got = crate::superagent::inbox::read(&dir, &board).unwrap();
        assert_eq!(got.len(), 1, "super's inbox reader accepts it");
        let names: Vec<_> = std::fs::read_dir(dir.join("inbox")).unwrap().collect();
        assert_eq!(names.len(), 1, "no temporary file left");
    }

    #[tokio::test]
    async fn unapplied_last_report_is_rewritten_on_resume() {
        let f = Fixture::new(false, Task::default());
        let first = f.report(json!({"state": "progress", "summary": "one"})).await;
        let second = f.report(json!({"state": "blocked", "summary": "two"})).await;
        let branch = [result_entry(&first), result_entry(&second)];
        assert_eq!(last_seq(&branch), 2);
        let id = f.tool.id.clone();
        let path = rewrite_unapplied(&branch, &id).expect("rewritten");
        assert_eq!(path, id.dir.join("inbox/T1-2.json"));
        let r: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(r, report(&second));
        // Once super applied it, nothing is written.
        std::fs::remove_file(&path).unwrap();
        let mut board = Board::load(&id.dir).unwrap();
        board.tasks[0].applied.push(r["id"].as_str().unwrap().into());
        board.save(&id.dir).unwrap();
        assert_eq!(rewrite_unapplied(&branch, &id), None);
        assert!(!path.exists());
    }

    #[test]
    fn report_only_at_depth_zero_with_an_identity() {
        fn vars(task: Option<&'static str>) -> impl Fn(&str) -> Option<String> {
            move |v: &str| match v {
                "NOVACODE_TASK" => task.map(String::from),
                "NOVACODE_TASK_TOKEN" => Some("tok".into()),
                "NOVACODE_SUPER_DIR" => Some("/s".into()),
                _ => None,
            }
        }
        let env = Identity::from_vars(vars(Some("T1")));
        assert_eq!(
            env,
            Some(Identity {
                task: "T1".into(),
                token: "tok".into(),
                dir: "/s".into()
            })
        );
        assert_eq!(Identity::from_vars(vars(None)), None);
        assert_eq!(Identity::resolve(0, env.clone(), None), env);
        assert_eq!(Identity::resolve(1, env.clone(), None), None, "no report in a subagent");
        // `novacode -r` without the variables: the saved entry brings it back.
        let saved = Identity::saved(&[entry(env.clone().unwrap().entry())]);
        assert_eq!(saved, env);
        assert_eq!(Identity::resolve(0, None, saved.clone()), env);
        assert_eq!(Identity::resolve(2, None, saved), None);
    }
}
