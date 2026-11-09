//! Repo notes (L10.4b): facts about one repository learned across sessions from failed
//! tool calls and the fixes that followed (a command spelling that works here, a tool that
//! is missing, a moved path, a file whose edits keep missing).
//!
//! Code reads the repo's finished session logs (its main checkout and worktrees) and keeps
//! the evidence in `<home>/memory/projects/<key>.notes.json`. A note is used once
//! [`CONFIRM`] distinct sessions show it and retired after [`RETIRE`] sessions contradict
//! it. Every [`MODEL_EVERY`] newly read sessions a cheap model rewords the notes and may
//! propose more from recent unexplained failures. Active notes are written to
//! `<repo>/.novacode/notes.md`; deleting a line there rejects that note for good, and
//! rewording one keeps the user's text. The prompt reads that file once at session start.

use crate::memory::Worker;
use crate::memstore::{project_key, project_root};
use crate::session::{Entry, EntryKind, SUBAGENT_TASK_PREFIX, Store};
use novacode_ai::adapter::{CacheRetention, Request};
use novacode_base::storage::{Durability, write_atomic_no_backup};
use novacode_types::{Block, Message, StopReason, StreamEvent, ToolDef, Usage};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use tokio_util::sync::CancellationToken;

/// Distinct sessions that must show a note before it is used.
pub const CONFIRM: usize = 3;
/// Sessions that must contradict a note before it is retired.
pub const RETIRE: usize = 2;
/// Newly read sessions between runs of the model step.
pub const MODEL_EVERY: u32 = 3;
/// Notes written to the file, most confirmed first.
const MAX_NOTES: usize = 20;
/// Character cap of the prompt section.
const MAX_CHARS: usize = 1500;
/// Unexplained failures kept for the model step.
const MAX_FAILURES: usize = 40;
/// Evidence entries kept; the least confirmed candidates go first.
const MAX_KEPT: usize = 500;
/// Session files read per run, oldest first; the rest wait for the next session.
const MAX_READ: usize = 200;
/// Calls after a failure in which its fix is looked for.
const WINDOW: usize = 4;

const HEADER: &str = "# Repo notes\n\nLearned by novacode from past sessions in this repo: suggestions, not rules.\n\
Delete a line to drop that note for good; edit its text to reword it.\n";

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Note {
    pub text: String,
    /// The user reworded it in the notes file: the model leaves it alone.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub user: bool,
    /// Sessions that showed it.
    #[serde(default)]
    pub seen: BTreeSet<String>,
    /// Sessions since the last confirmation that contradicted it.
    #[serde(default)]
    pub against: BTreeSet<String>,
    /// The user deleted its line: never written again.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub rejected: bool,
}

impl Note {
    pub fn active(&self) -> bool {
        !self.rejected && self.seen.len() >= CONFIRM && self.against.len() < RETIRE
    }
}

/// A failed call no rule explained, for the model step.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Failure {
    pub session: String,
    pub tool: String,
    pub input: String,
    pub error: String,
}

/// One repo's evidence.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct State {
    /// Session file -> its length when read; a grown file is read again (idempotent).
    #[serde(default)]
    pub read: BTreeMap<String, u64>,
    #[serde(default)]
    pub notes: BTreeMap<String, Note>,
    #[serde(default)]
    pub failures: Vec<Failure>,
    /// Sessions read since the model step last ran.
    #[serde(default)]
    pub unworded: u32,
    /// Keys last written to the notes file: one missing from it was deleted by the user.
    #[serde(default)]
    pub rendered: BTreeSet<String>,
}

/// What one session shows.
#[derive(Debug, Default, PartialEq)]
pub struct Evidence {
    /// (key, rule wording).
    pub confirm: Vec<(String, String)>,
    pub contradict: Vec<String>,
    pub failures: Vec<Failure>,
}

struct Call {
    tool: String,
    args: Value,
    ok: bool,
    out: String,
}

fn calls(entries: &[&Entry]) -> Vec<Call> {
    let mut pending: BTreeMap<String, Value> = BTreeMap::new();
    let mut out = vec![];
    for e in entries {
        let EntryKind::Message { message } = &e.kind else { continue };
        match message {
            Message::Assistant(a) => {
                for c in a.tool_calls() {
                    pending.insert(c.id.clone(), c.arguments.clone());
                }
            }
            Message::ToolResult {
                call_id,
                tool_name,
                content,
                is_error,
                ..
            } => {
                let text: Vec<&str> = content
                    .iter()
                    .filter_map(|b| match b {
                        Block::Text { text, .. } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect();
                out.push(Call {
                    tool: tool_name.clone(),
                    args: pending.remove(call_id).unwrap_or(Value::Null),
                    ok: !*is_error,
                    out: text.join("\n"),
                });
            }
            _ => {}
        }
    }
    out
}

fn is_var(k: &str) -> bool {
    k.len() > 1
        && k.starts_with(|c: char| c.is_ascii_uppercase())
        && k.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
}

/// A command's leading program words (`python -m pytest`, `npx tsc`, `cargo test`) and the
/// variables it sets inline or exports. `cd`/`export`/`source` segments are skipped.
fn head(cmd: &str) -> (String, Vec<String>) {
    let mut vars = vec![];
    let mut first: Option<String> = None;
    for seg in cmd.split(['&', ';', '|', '\n']).map(str::trim).filter(|s| !s.is_empty()) {
        let mut words = seg.split_whitespace();
        match words.clone().next() {
            Some("export") => {
                vars.extend(words.skip(1).filter_map(|w| w.split_once('=')).map(|(k, _)| k.to_string()));
                continue;
            }
            Some("cd" | "source" | "." | "set") => continue,
            _ => {}
        }
        let mut out: Vec<&str> = vec![];
        for w in words.by_ref() {
            if out.is_empty() && w == "env" {
                continue;
            }
            if out.is_empty()
                && let Some((k, _)) = w.split_once('=')
                && is_var(k)
            {
                vars.push(k.to_string());
                continue;
            }
            let after_m = out.last() == Some(&"-m");
            if !after_m && ((w.starts_with('-') && w != "-m") || w.contains(['/', '.', '=', '"', '\'', '$', '`', '(', '<', '>', '*'])) {
                break;
            }
            out.push(w);
            if out.len() == 4 || (out.len() == 3 && out[2] != "-m") {
                break;
            }
        }
        if out.last() == Some(&"-m") {
            out.pop();
        }
        if first.is_none() && !out.is_empty() {
            first = Some(out.join(" "));
        }
    }
    (first.unwrap_or_default(), vars)
}

static ABSENT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"([\w.+-]+): command not found|command not found: ([\w.+-]+)").unwrap());
static ENV: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\b([A-Z][A-Z0-9_]{2,})\b[^\n]{0,40}?\b(?:is not set|not set|is required|must be set|missing|undefined)")
        .unwrap()
});
static MARK: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^- (.*?)\s*<!-- note:(.+?) -->\s*$").unwrap());

fn absent(out: &str) -> Option<String> {
    let c = ABSENT.captures(out)?;
    c.get(1).or(c.get(2)).map(|m| m.as_str().to_string())
}

fn short(s: &str, n: usize) -> String {
    let s = s.trim();
    match s.char_indices().nth(n) {
        Some((i, _)) => format!("{}…", &s[..i]),
        None => s.to_string(),
    }
}

/// Keys go into an HTML comment in the notes file.
fn key(s: String) -> String {
    s.replace(['<', '>', '\n'], "")
}

/// The evidence one session's branch shows. Paths are made relative to the session's `cwd`,
/// so a worktree's session and the main checkout's agree.
pub fn scan(session: &str, cwd: &Path, entries: &[&Entry]) -> Evidence {
    let calls = calls(entries);
    // Relative to the checkout, so a worktree's session and the main checkout's agree;
    // `None` outside it.
    let rel = |p: &str| -> Option<String> {
        let r = Path::new(p).strip_prefix(cwd).unwrap_or(Path::new(p));
        if r.is_absolute() {
            return None;
        }
        let r = r.display().to_string();
        Some(match r.strip_prefix(".worktrees/").and_then(|w| w.split_once('/')) {
            Some((_, rest)) => rest.to_string(),
            None => r,
        })
    };
    let path_of = |c: &Call| c.args["path"].as_str().unwrap_or_default().to_string();
    let cmd_of = |c: &Call| c.args["command"].as_str().unwrap_or_default().to_string();
    let mut ev = Evidence::default();
    for (i, c) in calls.iter().enumerate() {
        let later = || calls[i + 1..].iter().take(WINDOW);
        let mut explained = false;
        match (c.tool.as_str(), c.ok) {
            ("bash", ok) => {
                let (h, _) = head(&cmd_of(c));
                if h.is_empty() {
                    continue;
                }
                let prog = h.split(' ').next().unwrap_or_default().to_string();
                if ok {
                    ev.contradict.push(key(format!("form:{h}")));
                    ev.contradict.push(key(format!("absent:{prog}")));
                    continue;
                }
                if let Some(p) = absent(&c.out) {
                    ev.confirm.push((key(format!("absent:{p}")), format!("`{p}` is not installed here.")));
                    explained = true;
                }
                if let Some(var) = ENV.captures(&c.out).map(|m| m[1].to_string())
                    && later().any(|b| b.tool == "bash" && b.ok && head(&cmd_of(b)).1.contains(&var))
                {
                    ev.confirm.push((key(format!("env:{var}")), format!("`{h}` needs `{var}` set.")));
                    explained = true;
                }
                // The fix wraps the failing words in order (`pytest` -> `python -m pytest`).
                let wraps = |b: &str| {
                    let mut ws = b.split(' ');
                    h.split(' ').all(|w| ws.any(|x| x == w))
                };
                if let Some(fix) = later()
                    .filter(|b| b.tool == "bash" && b.ok)
                    .map(|b| head(&cmd_of(b)).0)
                    .find(|b| *b != h && wraps(b))
                {
                    ev.confirm.push((key(format!("form:{h}")), format!("`{h}` fails here; `{fix}` works.")));
                    explained = true;
                }
            }
            ("edit", false) if c.out.contains("old_text not found") => {
                let Some(p) = rel(&path_of(c)) else { continue };
                ev.confirm.push((
                    key(format!("edit:{p}")),
                    format!("Edits to `{p}` often miss: read it again right before editing it."),
                ));
                explained = true;
            }
            ("read", true) => {
                if let Some(p) = rel(&path_of(c)) {
                    ev.contradict.push(key(format!("path:{p}")));
                }
            }
            ("read", false) if c.out.contains("does not exist") => {
                let p = path_of(c);
                let name = Path::new(&p).file_name().map(|n| n.to_os_string());
                if let Some(q) = later()
                    .filter(|b| b.tool == "read" && b.ok)
                    .map(path_of)
                    .find(|q| *q != p && Path::new(q).file_name().map(|n| n.to_os_string()) == name)
                    && let (Some(p), Some(q)) = (rel(&p), rel(&q))
                {
                    ev.confirm.push((key(format!("path:{p}")), format!("`{p}` does not exist; the file is `{q}`.")));
                    explained = true;
                }
            }
            (tool, false) if c.out.starts_with("invalid arguments") && later().any(|b| b.tool == tool && b.ok) => {
                // The error's kind, not the model's quoted input.
                let first = c.out.lines().next().unwrap_or_default();
                let e = short(first.split('"').next().unwrap_or(first).trim_end_matches([' ', ':']), 80);
                ev.confirm.push((
                    key(format!("args:{tool}:{e}")),
                    format!("`{tool}` calls often fail with `{e}`: check its parameters."),
                ));
                explained = true;
            }
            _ => {}
        }
        if !c.ok && !explained {
            let input = match c.tool.as_str() {
                "bash" => cmd_of(c),
                _ => c.args.to_string(),
            };
            ev.failures.push(Failure {
                session: session.to_string(),
                tool: c.tool.clone(),
                input: short(&input, 150),
                error: short(&c.out, 300),
            });
        }
    }
    let confirmed: BTreeSet<String> = ev.confirm.iter().map(|(k, _)| k.clone()).collect();
    ev.contradict.retain(|k| !confirmed.contains(k));
    ev.contradict.sort();
    ev.contradict.dedup();
    ev
}

impl State {
    pub fn path(home: &Path, root: &Path) -> PathBuf {
        home.join("memory")
            .join("projects")
            .join(format!("{}.notes.json", project_key(root)))
    }

    pub fn load(path: &Path) -> Self {
        std::fs::read(path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }

    /// Add one session's evidence. Confirmations are sets of session ids, so a session
    /// read twice changes nothing.
    pub fn fold(&mut self, session: &str, ev: Evidence) {
        for (k, text) in ev.confirm {
            let n = self.notes.entry(k).or_insert_with(|| Note {
                text,
                ..Default::default()
            });
            if n.seen.insert(session.to_string()) {
                n.against.clear();
            }
        }
        for k in ev.contradict {
            if let Some(n) = self.notes.get_mut(&k).filter(|n| !n.seen.contains(session)) {
                n.against.insert(session.to_string());
            }
        }
        self.failures.extend(ev.failures);
        let extra = self.failures.len().saturating_sub(MAX_FAILURES);
        self.failures.drain(..extra);
        self.unworded += 1;
        if self.notes.len() > MAX_KEPT {
            let mut weak: Vec<(usize, String)> = self
                .notes
                .iter()
                .filter(|(_, n)| !n.active() && !n.rejected)
                .map(|(k, n)| (n.seen.len(), k.clone()))
                .collect();
            weak.sort();
            for (_, k) in weak.into_iter().take(self.notes.len() - MAX_KEPT) {
                self.notes.remove(&k);
            }
        }
    }

    /// Take in the user's changes to the notes file: a deleted line rejects its note,
    /// a reworded one keeps the user's text.
    pub fn apply_file(&mut self, text: &str) {
        let found: BTreeMap<String, String> = text
            .lines()
            .filter_map(|l| MARK.captures(l))
            .map(|c| (c[2].to_string(), c[1].trim().to_string()))
            .collect();
        for k in &self.rendered {
            if !found.contains_key(k)
                && let Some(n) = self.notes.get_mut(k)
            {
                n.rejected = true;
            }
        }
        for (k, t) in found {
            if let Some(n) = self.notes.get_mut(&k).filter(|n| n.text != t && !t.is_empty()) {
                n.text = t;
                n.user = true;
            }
        }
    }

    fn active(&self) -> Vec<(&String, &Note)> {
        let mut a: Vec<(&String, &Note)> = self.notes.iter().filter(|(_, n)| n.active()).collect();
        a.sort_by(|x, y| y.1.seen.len().cmp(&x.1.seen.len()).then(x.0.cmp(y.0)));
        a.truncate(MAX_NOTES);
        a
    }

    /// The notes file, and the keys it holds.
    pub fn render(&self) -> (String, BTreeSet<String>) {
        let mut s = format!("{HEADER}\n");
        let mut keys = BTreeSet::new();
        for (k, n) in self.active() {
            let _ = writeln!(s, "- {} <!-- note:{k} -->", n.text.replace('\n', " "));
            keys.insert(k.clone());
        }
        (s, keys)
    }

    /// The model step: reword notes and propose new ones from recent failures.
    pub async fn word(&mut self, w: &Worker) -> Result<Usage, String> {
        let mut known: Vec<(&String, &Note)> = self
            .notes
            .iter()
            .filter(|(_, n)| !n.rejected && !n.user)
            .collect();
        known.sort_by_key(|(_, n)| std::cmp::Reverse(n.seen.len()));
        known.truncate(40);
        let mut body = String::from("Notes (key | sessions | text):\n");
        for (k, n) in &known {
            let _ = writeln!(body, "{k} | {} | {}", n.seen.len(), n.text);
        }
        body.push_str("\nRecent failures no rule explained (index | session | tool | input | error):\n");
        for (i, f) in self.failures.iter().enumerate() {
            let _ = writeln!(
                body,
                "{i} | {} | {} | {} | {}",
                f.session,
                f.tool,
                f.input.replace('\n', " "),
                f.error.replace('\n', " ")
            );
        }
        let req = Request {
            tail: Vec::new(),
            model: w.model.clone(),
            messages: vec![
                Message::System { text: WORDER_PROMPT.into() },
                Message::User { content: vec![Block::text(body)] },
            ],
            tools: vec![write_tool()],
            max_tokens: 4096,
            cache: CacheRetention::Short,
            thinking: None,
            images: false,
            session: None,
        };
        let mut rx = w.adapter.stream(req, CancellationToken::new());
        while let Some(ev) = rx.recv().await {
            match ev {
                StreamEvent::Done(m) if m.stop == StopReason::Length => return Err("cut off".into()),
                StreamEvent::Done(m) => {
                    for call in m.tool_calls().filter(|t| t.name == "write_notes") {
                        self.apply_words(&call.arguments);
                    }
                    self.failures.clear();
                    self.unworded = 0;
                    return Ok(m.usage);
                }
                StreamEvent::Error(m) => return Err(m.error_message.unwrap_or_else(|| format!("{:?}", m.stop))),
                _ => {}
            }
        }
        Err("stream ended without a result".into())
    }

    fn apply_words(&mut self, args: &Value) {
        let text = |v: &Value| {
            v["text"]
                .as_str()
                .map(str::trim)
                .filter(|t| !t.is_empty() && t.len() <= 300)
                .map(|t| t.replace('\n', " "))
        };
        for r in args["reword"].as_array().into_iter().flatten() {
            if let (Some(k), Some(t)) = (r["key"].as_str(), text(r))
                && let Some(n) = self.notes.get_mut(k).filter(|n| !n.user && !n.rejected)
            {
                n.text = t;
            }
        }
        for p in args["propose"].as_array().into_iter().flatten() {
            let Some(t) = text(p) else { continue };
            // Only sessions the digest shows count as evidence.
            let sessions: BTreeSet<String> = p["failures"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|i| self.failures.get(i.as_u64()? as usize))
                .map(|f| f.session.clone())
                .collect();
            if sessions.is_empty() {
                continue;
            }
            let n = self
                .notes
                .entry(format!("model:{}", crate::memory::note_id(&t.to_lowercase())))
                .or_insert_with(|| Note {
                    text: t,
                    ..Default::default()
                });
            n.seen.extend(sessions);
        }
    }
}

const WORDER_PROMPT: &str = "You keep short notes about one code repository for a coding agent, learned from its \
past sessions. A note is one line that saves a future session a failed attempt in this repo: a command spelling that \
works, a tool that is missing, the right path, a setup step, a file that needs care. Write a plain sentence of at most \
25 words; keep exact commands, paths and names in backticks. Never write about task progress, completion, general \
coding advice, or anything not specific to this repo. Call write_notes once: `reword` gives clearer text for listed \
notes by key (leave out the ones that are fine); `propose` adds a note only when the failures show a repeated, \
repo-specific cause, citing the failure indices that show it. Propose nothing when unsure.";

fn write_tool() -> ToolDef {
    ToolDef {
        name: "write_notes".into(),
        description: "Reword notes and propose new ones.".into(),
        schema: json!({
            "type": "object",
            "properties": {
                "reword": {"type": "array", "items": {"type": "object", "properties": {
                    "key": {"type": "string"}, "text": {"type": "string"}}, "required": ["key", "text"]}},
                "propose": {"type": "array", "items": {"type": "object", "properties": {
                    "text": {"type": "string"}, "failures": {"type": "array", "items": {"type": "integer"}}},
                    "required": ["text", "failures"]}}
            },
            "required": ["reword", "propose"]
        }),
    }
}

/// `<repo>/.novacode/notes.md`.
pub fn notes_path(root: &Path) -> PathBuf {
    root.join(".novacode").join("notes.md")
}

/// Read the repo's unread session files into `st`; how many were read. Subagent sessions,
/// other repos and session `current` are skipped.
fn read_sessions(st: &mut State, home: &Path, root: &Path, current: &str) -> usize {
    let slug = root.display().to_string().replace('/', "-");
    let nested = format!("{slug}-");
    let mut files: Vec<(std::time::SystemTime, PathBuf, u64)> = std::fs::read_dir(home.join("sessions"))
        .into_iter()
        .flatten()
        .flatten()
        .filter(|d| {
            let n = d.file_name().to_string_lossy().to_string();
            n == slug || n.starts_with(&nested)
        })
        .flat_map(|d| std::fs::read_dir(d.path()).into_iter().flatten().flatten())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "jsonl"))
        .filter_map(|p| {
            let m = std::fs::metadata(&p).ok()?;
            Some((m.modified().ok()?, p, m.len()))
        })
        .collect();
    files.sort();
    let mut n = 0;
    for (_, p, len) in files {
        let k = p.display().to_string();
        if st.read.get(&k) == Some(&len) {
            continue;
        }
        if n == MAX_READ {
            break;
        }
        let Ok(s) = Store::peek(&p) else {
            st.read.insert(k, len);
            continue;
        };
        if s.header.id == current {
            continue;
        }
        let branch = s.branch();
        let subagent = branch.iter().find_map(|e| match &e.kind {
            EntryKind::Message {
                message: Message::User { content },
            } => Some(content.iter().any(|b| matches!(b, Block::Text { text, .. } if text.starts_with(SUBAGENT_TASK_PREFIX)))),
            _ => None,
        });
        if s.header.cwd.starts_with(root) && subagent == Some(false) {
            let ev = scan(&s.header.id, &s.header.cwd, &branch);
            st.fold(&s.header.id, ev);
            n += 1;
        }
        st.read.insert(k, len);
    }
    n
}

/// One learning pass for `cwd`'s repo: take in the user's edits to the notes file, read
/// unread sessions, run the model step when due, and write the evidence and notes file.
/// Returns the sessions read.
pub async fn learn(home: PathBuf, cwd: PathBuf, current: String, worker: Option<Worker>) -> Result<usize, String> {
    let root = project_root(&cwd);
    let state = State::path(&home, &root);
    let notes = notes_path(&root);
    let (mut st, n) = tokio::task::spawn_blocking({
        let (state, notes, root) = (state.clone(), notes.clone(), root.clone());
        move || {
            let mut st = State::load(&state);
            if let Ok(t) = std::fs::read_to_string(&notes) {
                st.apply_file(&t);
            }
            let n = read_sessions(&mut st, &home, &root, &current);
            (st, n)
        }
    })
    .await
    .map_err(|e| e.to_string())?;
    if let Some(w) = worker.filter(|_| st.unworded >= MODEL_EVERY && (!st.failures.is_empty() || !st.notes.is_empty())) {
        // A failed model step leaves the rule wording; it runs again after the next session.
        let _ = st.word(&w).await;
    }
    let (text, keys) = st.render();
    let old = std::fs::read_to_string(&notes).ok();
    if old.as_deref() != Some(text.as_str()) && (old.is_some() || !keys.is_empty()) {
        if let Some(d) = notes.parent() {
            std::fs::create_dir_all(d).map_err(|e| e.to_string())?;
        }
        write_atomic_no_backup(&notes, text.as_bytes(), Durability::Fast).map_err(|e| e.to_string())?;
    }
    st.rendered = keys;
    if let Some(d) = state.parent() {
        std::fs::create_dir_all(d).map_err(|e| e.to_string())?;
    }
    let json = serde_json::to_vec(&st).map_err(|e| e.to_string())?;
    write_atomic_no_backup(&state, &json, Durability::Fast).map_err(|e| e.to_string())?;
    Ok(n)
}

/// The `repo_notes` prompt section: `cwd`'s repo notes file without its markers, capped.
pub fn section(cwd: &Path) -> Option<String> {
    let t = std::fs::read_to_string(notes_path(&project_root(cwd))).ok()?;
    let mut s = String::from(
        "Learned from past sessions in this repo. Suggestions, not rules: they may be outdated, and the user's \
         instructions win.",
    );
    let start = s.len();
    for l in t.lines().filter(|l| l.starts_with("- ")) {
        let l = MARK.captures(l).map_or(l[2..].trim().to_string(), |c| c[1].trim().to_string());
        if s.len() + l.len() + 3 > MAX_CHARS {
            break;
        }
        s.push_str("\n- ");
        s.push_str(&l);
    }
    (s.len() > start).then_some(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use novacode_types::{AssistantMessage, ToolCall};

    /// A session of (tool, args, ok, output) calls.
    fn entries(calls: &[(&str, Value, bool, &str)]) -> Vec<Entry> {
        let mut out = vec![];
        let mut parent: Option<String> = None;
        let mut push = |message: Message, out: &mut Vec<Entry>| {
            let id = format!("e{}", out.len());
            out.push(Entry {
                id: id.clone(),
                parent: parent.replace(id),
                ts_ms: 0,
                kind: EntryKind::Message { message },
            });
        };
        push(Message::User { content: vec![Block::text("fix it")] }, &mut out);
        for (i, (tool, args, ok, text)) in calls.iter().enumerate() {
            let call_id = format!("c{i}");
            push(
                Message::Assistant(AssistantMessage {
                    content: vec![Block::ToolCall(ToolCall {
                        id: call_id.clone(),
                        name: tool.to_string(),
                        arguments: args.clone(),
                        arguments_error: None,
                        replay: None,
                    })],
                    stop: StopReason::ToolUse,
                    usage: Usage::default(),
                    provider: "p".into(),
                    model: "m".into(),
                    response_id: None,
                    error_message: None,
                    thinking_level: None,
                }),
                &mut out,
            );
            push(
                Message::ToolResult {
                    call_id,
                    tool_name: tool.to_string(),
                    content: vec![Block::text(*text)],
                    is_error: !ok,
                    details: None,
                },
                &mut out,
            );
        }
        out
    }

    fn bash(cmd: &str) -> Value {
        json!({ "command": cmd })
    }

    fn keys(ev: &Evidence) -> Vec<&str> {
        ev.confirm.iter().map(|(k, _)| k.as_str()).collect()
    }

    #[test]
    fn heads_keep_the_program_words() {
        assert_eq!(head("cd app && python -m pytest tests/x.py -q").0, "python -m pytest");
        assert_eq!(head("npx tsc --noEmit").0, "npx tsc");
        assert_eq!(head("cargo test -p novacode 2>&1 | tail").0, "cargo test");
        assert_eq!(head("FOO=1 BAR_X=2 make build").0, "make build");
        assert_eq!(head("FOO=1 make").1, ["FOO"]);
        assert_eq!(head("export DATABASE_URL=x; npm run migrate").1, ["DATABASE_URL"]);
    }

    #[test]
    fn rules_pair_each_failure_with_its_fix() {
        let cwd = Path::new("/r");
        let e = entries(&[
            ("bash", bash("pytest tests/a.py"), false, "bash: pytest: command not found\n[exit code 127]"),
            ("bash", bash("python -m pytest tests/a.py"), true, "1 passed"),
            ("bash", bash("npm run migrate"), false, "Error: DATABASE_URL is not set\n[exit code 1]"),
            ("bash", bash("DATABASE_URL=x npm run migrate"), true, "ok"),
            ("edit", json!({"path": "/r/src/a.rs"}), false, "old_text not found (read the file again)"),
            ("read", json!({"path": "/r/src/lib/b.rs"}), false, "/r/src/lib/b.rs does not exist"),
            ("read", json!({"path": "/r/src/b.rs"}), true, "fn b() {}"),
            ("grep", json!({}), false, "invalid arguments: missing field `pattern`"),
            ("grep", json!({"pattern": "x"}), true, "a.rs:1"),
            ("bash", bash("make deploy"), false, "permission denied\n[exit code 2]"),
        ]);
        let ev = scan("s1", cwd, &e.iter().collect::<Vec<_>>());
        assert_eq!(
            keys(&ev),
            [
                "absent:pytest",
                "form:pytest",
                "env:DATABASE_URL",
                "edit:src/a.rs",
                "path:src/lib/b.rs",
                "args:grep:invalid arguments: missing field `pattern`",
            ]
        );
        assert!(ev.confirm[1].1.contains("`python -m pytest` works"), "{:?}", ev.confirm);
        assert!(ev.confirm[4].1.contains("the file is `src/b.rs`"), "{:?}", ev.confirm);
        // Only the unexplained failure goes to the model; successes contradict their heads.
        assert_eq!(ev.failures.len(), 1);
        assert_eq!(ev.failures[0].input, "make deploy");
        assert!(ev.contradict.contains(&"form:python -m pytest".to_string()));
        // A real test failure followed by an unrelated command is no note.
        let e = entries(&[
            ("bash", bash("cargo test"), false, "1 failed\n[exit code 101]"),
            ("bash", bash("cargo build"), true, ""),
            ("bash", bash("git rebase main"), false, "conflict"),
            ("bash", bash("git ls-tree main"), true, ""),
            ("edit", json!({"path": "/tmp/x.json"}), false, "old_text not found"),
        ]);
        assert!(scan("s", cwd, &e.iter().collect::<Vec<_>>()).confirm.is_empty());
        // A main-checkout session editing a worktree's file names it as the repo does.
        let e = entries(&[("edit", json!({"path": "/r/.worktrees/t1/src/a.rs"}), false, "old_text not found")]);
        assert_eq!(keys(&scan("s", cwd, &e.iter().collect::<Vec<_>>())), ["edit:src/a.rs"]);
    }

    fn ev(confirm: &[&str], contradict: &[&str]) -> Evidence {
        Evidence {
            confirm: confirm.iter().map(|k| (k.to_string(), format!("about {k}"))).collect(),
            contradict: contradict.iter().map(|k| k.to_string()).collect(),
            failures: vec![],
        }
    }

    #[test]
    fn three_sessions_confirm_two_retire_and_reading_twice_counts_once() {
        let mut st = State::default();
        st.fold("s1", ev(&["form:a"], &[]));
        st.fold("s1", ev(&["form:a"], &[]));
        st.fold("s2", ev(&["form:a"], &[]));
        assert!(!st.notes["form:a"].active(), "two sessions are not enough");
        st.fold("s3", ev(&["form:a"], &[]));
        assert!(st.notes["form:a"].active());
        st.fold("s4", ev(&[], &["form:a"]));
        assert!(st.notes["form:a"].active(), "one contradiction is not enough");
        st.fold("s5", ev(&[], &["form:a", "form:unknown"]));
        assert!(!st.notes["form:a"].active(), "retired");
        assert!(!st.notes.contains_key("form:unknown"), "contradictions never create notes");
        st.fold("s6", ev(&["form:a"], &[]));
        assert!(st.notes["form:a"].active(), "confirmed again");
    }

    #[test]
    fn the_file_takes_user_deletions_and_rewording() {
        let mut st = State::default();
        for s in ["s1", "s2", "s3"] {
            st.fold(s, ev(&["form:a", "form:b"], &[]));
        }
        let (text, keys) = st.render();
        assert!(text.starts_with("# Repo notes") && text.contains("- about form:a <!-- note:form:a -->"), "{text}");
        st.rendered = keys;
        let edited = text
            .lines()
            .filter(|l| !l.contains("note:form:b"))
            .map(|l| l.replace("about form:a", "Use `x` here."))
            .collect::<Vec<_>>()
            .join("\n");
        st.apply_file(&edited);
        assert!(st.notes["form:b"].rejected);
        assert_eq!(st.notes["form:a"].text, "Use `x` here.");
        assert!(st.notes["form:a"].user);
        st.fold("s4", ev(&["form:b"], &[]));
        let (text, _) = st.render();
        assert!(text.contains("Use `x` here.") && !text.contains("form:b"), "{text}");
    }

    #[test]
    fn model_words_known_notes_and_proposes_only_from_cited_sessions() {
        let mut st = State::default();
        st.fold("s1", ev(&["form:a"], &[]));
        st.notes.get_mut("form:a").unwrap().user = true;
        st.fold("s2", ev(&["form:c"], &[]));
        for s in ["s1", "s2", "s3"] {
            st.failures.push(Failure {
                session: s.into(),
                tool: "bash".into(),
                input: "make deploy".into(),
                error: "needs vpn".into(),
            });
        }
        st.apply_words(&json!({
            "reword": [{"key": "form:a", "text": "nope"}, {"key": "form:c", "text": "Run `c` first."}],
            "propose": [{"text": "`make deploy` needs the VPN.", "failures": [0, 1, 2, 9]}, {"text": "no evidence", "failures": []}]
        }));
        assert_eq!(st.notes["form:a"].text, "about form:a", "the user's wording stays");
        assert_eq!(st.notes["form:c"].text, "Run `c` first.");
        let m: Vec<&Note> = st.notes.iter().filter(|(k, _)| k.starts_with("model:")).map(|(_, n)| n).collect();
        assert_eq!(m.len(), 1);
        assert!(m[0].active(), "three cited sessions");
    }

    #[tokio::test]
    async fn learn_reads_the_repos_sessions_writes_the_file_and_the_section_reads_it() {
        let tmp = std::env::temp_dir().join(format!("nc-notes-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let (home, repo) = (tmp.join("home"), tmp.join("repo"));
        std::fs::create_dir_all(&repo).unwrap();
        let session = |cwd: &Path, id: &str, subagent: bool| {
            let dir = home.join("sessions").join(cwd.display().to_string().replace('/', "-"));
            std::fs::create_dir_all(&dir).unwrap();
            let mut e = entries(&[
                ("bash", bash("tsc --noEmit"), false, "tsc: command not found"),
                ("bash", bash("npx tsc --noEmit"), true, ""),
            ]);
            if subagent {
                e[0].kind = EntryKind::Message {
                    message: Message::User { content: vec![Block::text(format!("{SUBAGENT_TASK_PREFIX}x"))] },
                };
            }
            let h = crate::session::Header {
                kind: "session".into(),
                version: crate::session::VERSION,
                id: id.into(),
                cwd: cwd.to_path_buf(),
                created_ms: 0,
            };
            let mut text = serde_json::to_string(&h).unwrap() + "\n";
            for e in e {
                text += &(serde_json::to_string(&e).unwrap() + "\n");
            }
            std::fs::write(dir.join(format!("{id}.jsonl")), text).unwrap();
        };
        session(&repo, "s1", false);
        session(&repo.join(".worktrees/t1"), "s2", false);
        session(&repo, "sub", true);
        session(&tmp.join("repo-other"), "x", false);
        session(&repo, "now", false);
        let n = learn(home.clone(), repo.clone(), "now".into(), None).await.unwrap();
        assert_eq!(n, 2, "the worktree counts; subagent, other repo and current don't");
        assert!(!notes_path(&repo).exists(), "nothing active yet: no file");
        assert!(section(&repo).is_none());
        session(&repo, "s3", false);
        assert_eq!(learn(home.clone(), repo.clone(), "now".into(), None).await.unwrap(), 1);
        let file = std::fs::read_to_string(notes_path(&repo)).unwrap();
        assert!(file.contains("`tsc --noEmit`") || file.contains("`tsc`"), "{file}");
        let s = section(&repo).unwrap();
        assert!(s.contains("\n- `tsc` is not installed here.") && !s.contains("<!--"), "{s}");
        assert_eq!(learn(home.clone(), repo.clone(), "now".into(), None).await.unwrap(), 0, "nothing new");
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
