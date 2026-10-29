//! Session store (L4.1, L4.2, L4.4): an append-only JSONL tree, one file per
//! session. Entries are written once with single-write appends; a torn last
//! line is skipped on load. The file is created lazily at the first real
//! exchange. The projection is a pure function of the file (cache rule L1.7).

use novacode_base::storage::{self, FileLock};
use novacode_base::{new_id, paths::Paths};
use novacode_types::{Block, Message, typed_prompt};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

pub const VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Header {
    #[serde(rename = "type")]
    pub kind: String,
    pub version: u32,
    pub id: String,
    pub cwd: PathBuf,
    pub created_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EntryKind {
    Message {
        message: Message,
    },
    /// Hide (`replace: None`) or replace an earlier entry's message for the model (L4.6).
    /// The session moved to another folder (`/worktree`, enter_worktree); resume goes back there.
    Cwd {
        cwd: PathBuf,
    },
    ContextEdit {
        target: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        replace: Option<Message>,
    },
    ModelChange {
        provider: String,
        model: String,
    },
    ThinkingChange {
        level: novacode_types::ThinkingLevel,
    },
    SessionInfo {
        name: String,
    },
    /// Usage of a cache-warming replay (L2.17): billed, but not part of the conversation.
    WarmUsage {
        provider: String,
        model: String,
        usage: novacode_types::Usage,
    },
    /// The prompt sections a `System` message sets: the first has them all, later
    /// ones only the changed (`None` = removed) (L4.7). Not sent; the message is.
    Sections {
        patch: Vec<(String, Option<String>)>,
    },
    /// Day, branch and HEAD the model was last told about (L4.7). Not sent.
    Env {
        date: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        branch: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        head: Option<String>,
    },
    /// Observer notes about the conversation up to entry `covers` (L4.5). Not sent.
    Notes {
        notes: Vec<crate::memory::Note>,
        covers: String,
        #[serde(default)]
        usage: novacode_types::Usage,
        #[serde(default)]
        model: String,
    },
    /// Reflector output: lasting facts, up to the observer watermark `covers` (L4.5).
    Reflections {
        reflections: Vec<crate::memory::Reflection>,
        covers: String,
        #[serde(default)]
        usage: novacode_types::Usage,
        #[serde(default)]
        model: String,
    },
    /// Notes the dropper took out of active memory; they stay recallable (L4.5).
    Drops {
        ids: Vec<String>,
        #[serde(default)]
        usage: novacode_types::Usage,
        #[serde(default)]
        model: String,
    },
    /// The conversation before `first_kept` is replaced for the model by `system`
    /// (the full prompt at this point) and `summary` (L4.5). Old entries stay.
    Compaction {
        system: String,
        summary: String,
        first_kept: String,
        tokens_before: u64,
        #[serde(default)]
        notes: usize,
    },
    /// Files as they were at the start of the run of the prompt before it (L4.15):
    /// a git tree in the repo or shadow store. Not sent.
    Checkpoint {
        tree: String,
        /// Untracked files too big to snapshot.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        skipped: Vec<String>,
    },
    /// Outside git: a file's content before novacode first changed it (`None` =
    /// it didn't exist), as a blob in the shadow store (L4.15). Not sent.
    FileBefore {
        path: String,
        blob: Option<String>,
    },
    /// Moves the leaf (branch navigation). Not a tree node.
    Leaf {
        target: String,
    },
    /// An extension's own data (`novacode.state.append`), kept on the branch. Not sent.
    ExtState {
        ext: String,
        kind: String,
        data: serde_json::Value,
    },
    /// Injected text (nudges, reminders), sent as a `<system-reminder>` user message (L4.12).
    CustomMessage {
        kind: String,
        text: String,
        #[serde(default)]
        display: bool,
    },
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    pub id: String,
    pub parent: Option<String>,
    pub ts_ms: u64,
    #[serde(flatten)]
    pub kind: EntryKind,
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

pub struct Store {
    pub header: Header,
    /// `None` = ephemeral (`--no-session`), never written.
    path: Option<PathBuf>,
    entries: Vec<Entry>,
    index: HashMap<String, usize>,
    leaf: Option<String>,
    on_disk: bool,
    /// Entries written to the file; the rest wait for [`Store::flush`] after a failed write (L4.16).
    saved: usize,
    /// The last write failed: the file may end in a cut-off line.
    failed: bool,
    _lock: Option<FileLock>,
}

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("session file: {0}")]
    Io(#[from] io::Error),
    #[error("session {0} is open in another novacode process")]
    Busy(String),
    #[error("bad session header in {0}")]
    BadHeader(PathBuf),
    #[error("no session matches `{0}`")]
    NotFound(String),
}

/// End a line a failed write may have cut off, so the next entry starts on its own line.
fn end_line(path: &Path) -> io::Result<()> {
    let mut f = std::fs::OpenOptions::new().read(true).append(true).open(path)?;
    if f.seek(SeekFrom::End(-1)).is_ok() {
        let mut last = [0u8; 1];
        f.read_exact(&mut last)?;
        if last[0] != b'\n' {
            f.write_all(b"\n")?;
        }
    }
    Ok(())
}

/// Folder for one cwd: `<home>/sessions/<cwd with / as ->/`.
pub fn dir_for(paths: &Paths, cwd: &Path) -> PathBuf {
    paths
        .home()
        .join("sessions")
        .join(cwd.display().to_string().replace('/', "-"))
}

impl Store {
    /// A new session. Nothing touches disk until the first assistant message.
    pub fn create(paths: &Paths, cwd: &Path, ephemeral: bool) -> Self {
        let id = new_id().to_string();
        let path = (!ephemeral).then(|| dir_for(paths, cwd).join(format!("{id}.jsonl")));
        Self {
            header: Header {
                kind: "session".into(),
                version: VERSION,
                id,
                cwd: cwd.to_path_buf(),
                created_ms: now_ms(),
            },
            path,
            entries: Vec::new(),
            index: HashMap::new(),
            leaf: None,
            on_disk: false,
            saved: 0,
            failed: false,
            _lock: None,
        }
    }

    pub fn open(path: &Path) -> Result<Self, StoreError> {
        let lock = lock_session(path)?.ok_or_else(|| StoreError::Busy(path.display().to_string()))?;
        let mut f = std::fs::OpenOptions::new().read(true).append(true).open(path)?;
        let mut text = String::new();
        f.read_to_string(&mut text)?;
        // A crash mid-append leaves a line without '\n'; end it so the next append starts clean.
        if !text.is_empty() && !text.ends_with('\n') {
            f.seek(SeekFrom::End(0))?;
            f.write_all(b"\n")?;
        }
        let mut lines = text.lines();
        let header: Header = lines
            .next()
            .and_then(|l| serde_json::from_str(l).ok())
            .filter(|h: &Header| h.kind == "session")
            .ok_or_else(|| StoreError::BadHeader(path.to_path_buf()))?;
        let mut s = Self {
            header,
            path: Some(path.to_path_buf()),
            entries: Vec::new(),
            index: HashMap::new(),
            leaf: None,
            on_disk: true,
            saved: 0,
            failed: false,
            _lock: Some(lock),
        };
        for (n, line) in lines.enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<Entry>(line) {
                // A retried write whose first attempt did land: keep the first copy.
                Ok(e) if s.index.contains_key(&e.id) => {}
                Ok(e) => s.push(e),
                Err(e) => {
                    tracing::warn!(path = %path.display(), line = n + 2, error = %e, "skipping unreadable session line")
                }
            }
        }
        s.saved = s.entries.len();
        Ok(s)
    }

    fn push(&mut self, e: Entry) {
        match &e.kind {
            EntryKind::Leaf { target } => self.leaf = Some(target.clone()),
            _ => self.leaf = Some(e.id.clone()),
        }
        self.index.insert(e.id.clone(), self.entries.len());
        self.entries.push(e);
    }

    pub fn id(&self) -> &str {
        &self.header.id
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    /// Append a child of the current leaf and move the leaf to it.
    pub fn append(&mut self, kind: EntryKind) -> Result<String, StoreError> {
        let e = Entry {
            id: new_id().to_string(),
            parent: self.leaf.clone(),
            ts_ms: now_ms(),
            kind,
        };
        let id = e.id.clone();
        let first_exchange = matches!(
            &e.kind,
            EntryKind::Message {
                message: Message::Assistant(_)
            }
        );
        self.push(e);
        if self.on_disk || first_exchange || self.failed {
            self.flush()?;
        }
        Ok(id)
    }

    /// Write every entry not on disk yet, in order (L4.16). A failed write keeps
    /// the entries in memory; the next append or flush writes them first, so no
    /// entry on disk ever points at a parent that isn't.
    pub fn flush(&mut self) -> Result<(), StoreError> {
        let Some(path) = self.path.clone() else { return Ok(()) };
        let r = if self.on_disk {
            self.write_pending(&path)
        } else {
            self.materialise(&path)
        };
        self.failed = r.is_err();
        r
    }

    /// Entries a failed write left unsaved (0 while lazy creation hasn't written yet).
    pub fn unsaved(&self) -> usize {
        if self.failed {
            self.entries.len() - self.saved
        } else {
            0
        }
    }

    fn write_pending(&mut self, path: &Path) -> Result<(), StoreError> {
        if self.failed {
            end_line(path)?;
        }
        let wrote = self.saved < self.entries.len();
        while self.saved < self.entries.len() {
            storage::append_jsonl(path, &self.entries[self.saved])?;
            self.saved += 1;
        }
        // L4.17: on disk before the run goes on, so a power loss keeps it.
        // Windows flushes only through a handle with write access.
        if wrote {
            std::fs::OpenOptions::new().append(true).open(path)?.sync_data()?;
        }
        Ok(())
    }

    /// Write the header and everything so far, creating the file (lazy creation).
    fn materialise(&mut self, path: &Path) -> Result<(), StoreError> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let lock = lock_session(path)?.ok_or_else(|| StoreError::Busy(self.header.id.clone()))?;
        let mut buf = serde_json::to_string(&self.header).map_err(io::Error::other)?;
        buf.push('\n');
        for e in &self.entries {
            buf.push_str(&serde_json::to_string(e).map_err(io::Error::other)?);
            buf.push('\n');
        }
        // Truncate, not create_new: a failed first attempt may have left part of this new file.
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)?;
        f.write_all(buf.as_bytes())?;
        f.sync_data()?;
        // Windows can't open a folder as a file; NTFS journals the creation itself.
        #[cfg(unix)]
        if let Some(dir) = path.parent() {
            std::fs::File::open(dir)?.sync_all()?;
        }
        self.on_disk = true;
        self.saved = self.entries.len();
        self._lock = Some(lock);
        Ok(())
    }

    /// Move the leaf to `target` (L4.3): the next message branches from there.
    pub fn set_leaf(&mut self, target: &str) -> Result<(), StoreError> {
        if !self.index.contains_key(target) {
            return Err(StoreError::NotFound(target.into()));
        }
        self.append(EntryKind::Leaf { target: target.into() })?;
        Ok(())
    }

    /// A new session holding the root→`at` path of this one (L4.3 fork and
    /// clone), written at once. Entry ids are kept, so notes and recall still work.
    pub fn fork(&self, paths: &Paths, at: &str) -> Result<Store, StoreError> {
        let mut path: Vec<Entry> = Vec::new();
        let mut cur = Some(at);
        while let Some(id) = cur {
            let i = *self.index.get(id).ok_or_else(|| StoreError::NotFound(id.into()))?;
            path.push(self.entries[i].clone());
            cur = self.entries[i].parent.as_deref();
        }
        path.reverse();
        let mut s = Store::create(paths, &self.header.cwd, self.path.is_none());
        for e in path {
            s.push(e);
        }
        if let Some(p) = s.path.clone() {
            s.materialise(&p)?;
        }
        Ok(s)
    }

    /// The current branch, root first.
    pub fn branch(&self) -> Vec<&Entry> {
        self.path_to_leaf()
    }

    fn path_to_leaf(&self) -> Vec<&Entry> {
        let mut out = Vec::new();
        let mut cur = self.leaf.as_deref();
        while let Some(id) = cur {
            let Some(&i) = self.index.get(id) else { break };
            out.push(&self.entries[i]);
            cur = self.entries[i].parent.as_deref();
        }
        out.reverse();
        out
    }

    /// The messages sent to the model (L4.2). Pure: same file, same output.
    pub fn projection(&self) -> Vec<Message> {
        project(&self.path_to_leaf(), |_| true)
    }

    /// What a window replays: the projection without the hidden notes that never
    /// showed live (hooks, env changes, docs briefs, nudges); process, schedule and
    /// subagent notes stay, as one line each.
    pub fn transcript(&self) -> Vec<Message> {
        project(&self.path_to_leaf(), |kind| {
            ["process", "schedule", "agent"].contains(&kind)
        })
    }

    /// The last request that got a reply with usage, as far as the file can tell:
    /// the messages sent, the route, the reply's usage and its time. Lets a resumed
    /// session's first reply be checked for a cache miss (L2.17).
    pub fn last_request(&self) -> Option<(Vec<Message>, String, String, novacode_types::Usage, u64)> {
        let path = self.path_to_leaf();
        let i = path.iter().rposition(
            |e| matches!(&e.kind, EntryKind::Message { message: Message::Assistant(a) } if a.usage.prompt_total() > 0),
        )?;
        let EntryKind::Message {
            message: Message::Assistant(a),
        } = &path[i].kind
        else {
            return None;
        };
        let (provider, model) = path[..i].iter().rev().find_map(|e| match &e.kind {
            EntryKind::ModelChange { provider, model } => Some((provider.clone(), model.clone())),
            _ => None,
        })?;
        Some((project(&path[..i], |_| true), provider, model, a.usage, path[i].ts_ms))
    }

    /// Last recorded thinking level.
    pub fn last_thinking(&self) -> Option<novacode_types::ThinkingLevel> {
        self.entries.iter().rev().find_map(|e| match &e.kind {
            EntryKind::ThinkingChange { level } => Some(*level),
            _ => None,
        })
    }

    /// The prompt sections the model has now, folded along the current branch.
    pub fn sections(&self) -> crate::prompt::Sections {
        let mut out: crate::prompt::Sections = Vec::new();
        for e in self.path_to_leaf() {
            if let EntryKind::Sections { patch } = &e.kind {
                for (name, text) in patch {
                    let at = out.iter().position(|(n, _)| n == name);
                    match (at, text) {
                        (Some(i), Some(t)) => out[i].1 = t.clone(),
                        (Some(i), None) => {
                            out.remove(i);
                        }
                        (None, Some(t)) => out.push((name.clone(), t.clone())),
                        (None, None) => {}
                    }
                }
            }
        }
        out
    }

    /// The environment the model was last told about.
    pub fn last_env(&self) -> Option<crate::prompt::EnvState> {
        self.path_to_leaf().into_iter().rev().find_map(|e| match &e.kind {
            EntryKind::Env { date, branch, head } => Some(crate::prompt::EnvState {
                date: date.clone(),
                branch: branch.clone(),
                head: head.clone(),
            }),
            _ => None,
        })
    }

    /// Folder to resume in: the newest recorded move that still exists, else the start folder.
    pub fn last_cwd(&self) -> PathBuf {
        self.path_to_leaf()
            .into_iter()
            .rev()
            .find_map(|e| match &e.kind {
                EntryKind::Cwd { cwd } if cwd.is_dir() => Some(cwd.clone()),
                _ => None,
            })
            .unwrap_or_else(|| self.header.cwd.clone())
    }

    /// Last recorded model, as (provider, model).
    pub fn last_model(&self) -> Option<(String, String)> {
        self.entries.iter().rev().find_map(|e| match &e.kind {
            EntryKind::ModelChange { provider, model } => Some((provider.clone(), model.clone())),
            _ => None,
        })
    }
}

/// Messages of a path, with context edits applied (L4.2).
/// After the last compaction: its prompt, then the kept entries with the
/// summary placed in front of the first kept user message (L4.5). System
/// deltas older than the compaction are already part of its prompt.
fn project(path: &[&Entry], custom: impl Fn(&str) -> bool) -> Vec<Message> {
    let mut edits: HashMap<&str, Option<&Message>> = HashMap::new();
    for e in path {
        if let EntryKind::ContextEdit { target, replace } = &e.kind {
            edits.insert(target, replace.as_ref());
        }
    }
    let mut out = Vec::new();
    let compaction = path.iter().enumerate().rev().find_map(|(i, e)| match &e.kind {
        EntryKind::Compaction {
            system,
            summary,
            first_kept,
            ..
        } => Some((i, system, summary, first_kept)),
        _ => None,
    });
    let mut path: Vec<&Entry> = path.to_vec();
    let mut summary = None;
    if let Some((ci, system, text, first_kept)) = compaction
        && let Some(k) = path.iter().position(|e| &e.id == first_kept)
    {
        out.push(Message::System { text: system.clone() });
        summary = Some(text.clone());
        path = path
            .iter()
            .enumerate()
            .filter(|(i, e)| {
                *i >= k
                    && !(*i < ci
                        && matches!(
                            e.kind,
                            EntryKind::Message {
                                message: Message::System { .. }
                            }
                        ))
            })
            .map(|(_, e)| *e)
            .collect();
    }
    for e in path {
        let msg = match &e.kind {
            EntryKind::Message { message } => message.clone(),
            EntryKind::CustomMessage { kind, .. } if !custom(kind) => continue,
            EntryKind::CustomMessage { text, .. } => Message::User {
                content: vec![Block::text(format!("<system-reminder>\n{text}\n</system-reminder>"))],
            },
            _ => continue,
        };
        let mut msg = match edits.get(e.id.as_str()) {
            Some(None) => continue,
            Some(Some(r)) => (*r).clone(),
            None => msg,
        };
        // The summary opens the first kept user message; a cut at an assistant
        // message (split turn) gets a user message of its own before it.
        if !matches!(msg, Message::System { .. })
            && let Some(s) = summary.take()
        {
            let b = Block::text(format!("<compaction-summary>\n{s}\n</compaction-summary>"));
            match &mut msg {
                Message::User { content } => content.insert(0, b),
                _ => out.push(Message::User { content: vec![b] }),
            }
        }
        out.push(msg);
    }
    out
}

/// One user prompt in the session tree, for `/tree` and `/rewind` (L4.3).
#[derive(Debug, Clone, PartialEq)]
pub struct PromptNode {
    pub id: String,
    /// Where the leaf goes to redo this prompt: its parent.
    pub parent: Option<String>,
    /// Indent: the number of branch points above it.
    pub depth: usize,
    pub text: String,
    /// On the current branch.
    pub on_branch: bool,
    pub ts_ms: u64,
}

impl Store {
    /// Every user prompt in the file as a tree, depth-first, older siblings first.
    pub fn prompts(&self) -> Vec<PromptNode> {
        let on: std::collections::HashSet<&str> = self.path_to_leaf().iter().map(|e| e.id.as_str()).collect();
        let prompt_text = |e: &Entry| match &e.kind {
            EntryKind::Message {
                message: Message::User { content },
            } => Some(
                content
                    .iter()
                    .find_map(|b| match b {
                        Block::Text { text, .. } => Some(typed_prompt(text).lines().next().unwrap_or("").to_string()),
                        _ => None,
                    })
                    .unwrap_or_else(|| "[image]".into()),
            ),
            _ => None,
        };
        // Each prompt's nearest prompt ancestor.
        let up = |e: &Entry| {
            let mut cur = e.parent.as_deref();
            while let Some(id) = cur {
                // A parent lost to an unreadable line ends the walk, as in `path_to_leaf`.
                let &i = self.index.get(id)?;
                let p = &self.entries[i];
                if prompt_text(p).is_some() {
                    return Some(p.id.as_str());
                }
                cur = p.parent.as_deref();
            }
            None
        };
        let mut kids: BTreeMap<Option<&str>, Vec<&Entry>> = BTreeMap::new();
        for e in &self.entries {
            if prompt_text(e).is_some() {
                kids.entry(up(e)).or_default().push(e);
            }
        }
        let mut out = Vec::new();
        let mut stack: Vec<(&Entry, usize)> = kids.get(&None).into_iter().flatten().rev().map(|e| (*e, 0)).collect();
        while let Some((e, depth)) = stack.pop() {
            out.push(PromptNode {
                id: e.id.clone(),
                parent: e.parent.clone(),
                depth,
                text: prompt_text(e).unwrap_or_default(),
                on_branch: on.contains(e.id.as_str()),
                ts_ms: e.ts_ms,
            });
            let ch = kids.get(&Some(e.id.as_str())).cloned().unwrap_or_default();
            let d = if ch.len() > 1 { depth + 1 } else { depth };
            stack.extend(ch.into_iter().rev().map(|c| (c, d)));
        }
        out
    }
}

/// Session files for a cwd, newest (last written) first.
pub fn list(paths: &Paths, cwd: &Path) -> Vec<PathBuf> {
    let mut files: Vec<(SystemTime, PathBuf)> = std::fs::read_dir(dir_for(paths, cwd))
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "jsonl"))
        .filter_map(|p| Some((std::fs::metadata(&p).ok()?.modified().ok()?, p)))
        .collect();
    files.sort_by_key(|f| std::cmp::Reverse(f.0));
    files.into_iter().map(|(_, p)| p).collect()
}

/// How a subagent's first prompt starts (written by the parent); its session isn't the user's history.
pub const SUBAGENT_TASK_PREFIX: &str = "You are subagent ";

/// Typed prompts from this cwd's recent sessions, oldest first, at most `max` (L9.5 history).
/// Subagent sessions (same folder, first prompt from the parent) are skipped.
/// Newest sessions are read first (at most 20 user sessions), skipping session `exclude`.
/// Prompt text = the text blocks of stored user messages, joined as the TUI joins them.
pub fn recent_prompts(paths: &Paths, cwd: &Path, exclude: Option<&str>, max: usize) -> Vec<String> {
    let mut chunks: Vec<Vec<String>> = Vec::new();
    let (mut have, mut sessions) = (0, 0);
    let files = list(paths, cwd)
        .into_iter()
        .filter(|p| exclude.is_none_or(|x| p.file_stem().is_none_or(|s| s != x)));
    for path in files {
        if have >= max || sessions >= 20 {
            break;
        }
        let Ok(bytes) = std::fs::read(&path) else { continue };
        let mut found: Vec<String> = bytes
            .split(|b| *b == b'\n')
            .filter_map(|l| std::str::from_utf8(l).ok())
            // Cheap prefilter: only user messages are parsed.
            .filter(|l| l.contains("\"role\":\"user\""))
            .filter_map(|l| match serde_json::from_str::<Entry>(l).ok()?.kind {
                EntryKind::Message {
                    message: Message::User { content },
                } => Some(
                    content
                        .iter()
                        .filter_map(|b| match b {
                            Block::Text { text, .. } => Some(text.as_str()),
                            _ => None,
                        })
                        .collect::<String>(),
                ),
                _ => None,
            })
            .map(|t| typed_prompt(&t).into_owned())
            .filter(|t| !t.trim().is_empty())
            .collect();
        // Skipped before counting, so subagent sessions can't crowd out the user's.
        if found.first().is_some_and(|t| t.starts_with(SUBAGENT_TASK_PREFIX)) {
            continue;
        }
        sessions += 1;
        let keep = found.len().min(max - have);
        found.drain(..found.len() - keep);
        have += keep;
        chunks.push(found);
    }
    chunks.into_iter().rev().flatten().collect()
}

/// What the session picker shows.
#[derive(Debug, Clone, PartialEq)]
pub struct Summary {
    pub path: PathBuf,
    pub id: String,
    /// Session name, else the first prompt.
    pub title: String,
    pub prompts: usize,
    pub modified: SystemTime,
}

/// Read a session file without opening (locking) it.
pub fn summary(path: &Path) -> Option<Summary> {
    let text = std::fs::read_to_string(path).ok()?;
    let mut lines = text.lines();
    let header: Header = serde_json::from_str(lines.next()?).ok()?;
    let (mut name, mut first, mut prompts) = (None, None, 0);
    for l in lines {
        match serde_json::from_str::<Entry>(l).map(|e| e.kind) {
            Ok(EntryKind::SessionInfo { name: n }) => name = Some(n),
            Ok(EntryKind::Message {
                message: Message::User { content },
            }) => {
                prompts += 1;
                if first.is_none() {
                    first = content.iter().find_map(|b| match b {
                        Block::Text { text, .. } => Some(typed_prompt(text).lines().next().unwrap_or("").to_string()),
                        _ => None,
                    });
                }
            }
            _ => {}
        }
    }
    Some(Summary {
        path: path.to_path_buf(),
        id: header.id,
        title: name.or(first).unwrap_or_default(),
        prompts,
        modified: std::fs::metadata(path).ok()?.modified().ok()?,
    })
}

/// Find a session by id prefix: this cwd first, then every cwd.
/// The session's lock, retried for up to ~100 ms: a child forked by another
/// thread (bash, git) briefly holds a copy of a just-released lock until it execs.
fn lock_session(path: &Path) -> io::Result<Option<storage::FileLock>> {
    for _ in 0..5 {
        if let Some(l) = storage::try_lock(path)? {
            return Ok(Some(l));
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    storage::try_lock(path)
}

pub fn find(paths: &Paths, cwd: &Path, prefix: &str) -> Result<PathBuf, StoreError> {
    // A session file path works too (tmux restore stores the file, L8.14).
    if prefix.ends_with(".jsonl") && Path::new(prefix).is_file() {
        return Ok(PathBuf::from(prefix));
    }
    let hit = |dir: &Path| -> Option<PathBuf> {
        std::fs::read_dir(dir).ok()?.flatten().map(|e| e.path()).find(|p| {
            p.extension().is_some_and(|x| x == "jsonl")
                && p.file_stem().is_some_and(|s| s.to_string_lossy().starts_with(prefix))
        })
    };
    if let Some(p) = hit(&dir_for(paths, cwd)) {
        return Ok(p);
    }
    std::fs::read_dir(paths.home().join("sessions"))
        .into_iter()
        .flatten()
        .flatten()
        .find_map(|d| hit(&d.path()))
        .ok_or_else(|| StoreError::NotFound(prefix.into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(t: &str) -> EntryKind {
        EntryKind::Message {
            message: Message::user(t),
        }
    }

    fn reply(t: &str) -> EntryKind {
        EntryKind::Message {
            message: Message::Assistant(novacode_types::AssistantMessage {
                content: vec![Block::text(t)],
                stop: novacode_types::StopReason::Stop,
                usage: Default::default(),
                provider: "faux".into(),
                model: "m".into(),
                response_id: None,
                error_message: None,
                thinking_level: None,
            }),
        }
    }

    #[test]
    fn branch_tree_and_fork() {
        let home = tempfile::tempdir().unwrap();
        let paths = Paths::new(home.path());
        let mut s = Store::create(&paths, home.path(), false);
        s.append(msg("one")).unwrap();
        s.append(reply("r1")).unwrap();
        let two = s.append(msg("two")).unwrap();
        s.append(reply("r2")).unwrap();
        // Redo "two" differently: leaf back to its parent, then a new prompt.
        let parent = s
            .entries()
            .iter()
            .find(|e| e.id == two)
            .unwrap()
            .parent
            .clone()
            .unwrap();
        s.set_leaf(&parent).unwrap();
        s.append(msg("two again")).unwrap();
        s.append(reply("r2b")).unwrap();
        // "one" has two prompt children, so they indent.
        let t: Vec<(usize, String, bool)> = s
            .prompts()
            .iter()
            .map(|p| (p.depth, p.text.clone(), p.on_branch))
            .collect();
        assert_eq!(
            t,
            [
                (0, "one".into(), true),
                (1, "two".into(), false),
                (1, "two again".into(), true)
            ]
        );
        assert!(!format!("{:?}", s.projection()).contains("\"two\""));
        // A reopened file lands on the same branch.
        let file = s.path().unwrap().to_path_buf();
        drop(s);
        let s = Store::open(&file).unwrap();
        assert!(format!("{:?}", s.projection()).contains("two again"));
        // Fork at "one"'s reply: a new file with just that path.
        let at = s.prompts()[0].id.clone();
        let f = s.fork(&paths, &at).unwrap();
        assert_ne!(f.id(), s.id());
        assert_eq!(f.projection().len(), 1);
        assert!(f.path().unwrap().exists());
    }

    #[test]
    fn resume_follows_the_last_existing_folder() {
        let home = tempfile::tempdir().unwrap();
        let wt = tempfile::tempdir().unwrap();
        let paths = Paths::new(home.path());
        let start = home.path().to_path_buf();
        let mut s = Store::create(&paths, &start, false);
        s.append(msg("hi")).unwrap();
        s.append(reply("hello")).unwrap();
        assert_eq!(s.last_cwd(), start);
        s.append(EntryKind::Cwd { cwd: wt.path().into() }).unwrap();
        s.append(EntryKind::Cwd {
            cwd: "/gone/worktree".into(),
        })
        .unwrap();
        let file = s.path().unwrap().to_path_buf();
        drop(s);
        let s = Store::open(&file).unwrap();
        // A removed worktree is skipped for the newest folder that still exists.
        assert_eq!(s.last_cwd(), wt.path());
    }

    /// L4.16: a failed write keeps entries in memory and the next one writes them in
    /// order, past a cut-off line; reopening gives the whole branch, each entry once.
    #[cfg(unix)]
    #[test]
    fn failed_writes_are_written_later_in_order() {
        use std::os::unix::fs::PermissionsExt;
        let home = tempfile::tempdir().unwrap();
        let paths = Paths::new(home.path());
        let mut s = Store::create(&paths, Path::new("/w/proj"), false);
        s.append(msg("hi")).unwrap();
        s.append(reply("hello")).unwrap();
        let path = s.path().unwrap().to_path_buf();
        let mode = |m: u32| std::fs::set_permissions(&path, std::fs::Permissions::from_mode(m)).unwrap();
        mode(0o444);
        assert!(s.append(msg("lost?")).is_err());
        assert!(s.append(reply("still lost?")).is_err());
        assert_eq!(s.unsaved(), 2);
        mode(0o644);
        // What a write cut off mid-line looks like.
        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"{\"id\":\"cut")
            .unwrap();
        s.append(msg("after")).unwrap();
        assert_eq!(s.unsaved(), 0);
        // The same entry written twice (a write reported failed that did land) loads once.
        let last = std::fs::read_to_string(&path)
            .unwrap()
            .lines()
            .last()
            .unwrap()
            .to_string();
        storage::append_line(&path, &last).unwrap();
        let want = s.projection();
        drop(s);
        let s2 = Store::open(&path).unwrap();
        assert_eq!(s2.projection(), want);
        assert_eq!(s2.branch().len(), 5);
        assert_eq!(s2.entries().len(), 5);
    }

    #[test]
    fn lazy_creation_projection_and_reload() {
        let home = tempfile::tempdir().unwrap();
        let paths = Paths::new(home.path());
        let cwd = Path::new("/w/proj");
        let mut s = Store::create(&paths, cwd, false);
        s.append(msg("hi")).unwrap();
        let path = s.path().unwrap().to_path_buf();
        assert!(!path.exists(), "no file before the first exchange");
        s.append(reply("hello")).unwrap();
        assert!(path.exists());
        let failed = s.append(reply("partial")).unwrap();
        s.append(EntryKind::ContextEdit {
            target: failed,
            replace: None,
        })
        .unwrap();
        s.append(EntryKind::CustomMessage {
            kind: "nudge".into(),
            text: "go on".into(),
            display: false,
        })
        .unwrap();
        let want = s.projection();
        assert_eq!(want.len(), 3);
        // A window replays no hidden nudge.
        assert_eq!(s.transcript().len(), 2);
        drop(s);
        let s2 = Store::open(&path).unwrap();
        assert_eq!(s2.projection(), want);
        assert!(
            matches!(Store::open(&path), Err(StoreError::Busy(_))),
            "second opener is refused"
        );
        assert_eq!(list(&paths, cwd), vec![path.clone()]);
        assert_eq!(find(&paths, Path::new("/elsewhere"), &s2.id()[..8]).unwrap(), path);
    }

    #[test]
    fn torn_tail_is_skipped_and_appends_continue() {
        let home = tempfile::tempdir().unwrap();
        let paths = Paths::new(home.path());
        let mut s = Store::create(&paths, Path::new("/w"), false);
        s.append(msg("a")).unwrap();
        s.append(reply("b")).unwrap();
        let path = s.path().unwrap().to_path_buf();
        drop(s);
        // Simulate a crash mid-append.
        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(br#"{"id":"x","parent":"#)
            .unwrap();
        let mut s = Store::open(&path).unwrap();
        assert_eq!(s.projection().len(), 2);
        s.append(msg("c")).unwrap();
        drop(s);
        let s = Store::open(&path).unwrap();
        let p = s.projection();
        assert_eq!(p.len(), 3);
        assert_eq!(p[2], Message::user("c"));
    }

    #[test]
    fn prompts_survive_a_corrupt_middle_line() {
        let home = tempfile::tempdir().unwrap();
        let mut s = Store::create(&Paths::new(home.path()), home.path(), false);
        s.append(msg("one")).unwrap();
        let r1 = s.append(reply("r1")).unwrap();
        s.append(msg("two")).unwrap();
        s.append(reply("r2")).unwrap();
        let path = s.path().unwrap().to_path_buf();
        drop(s);
        // Break the line "two" points at, so its parent id is missing on open.
        let text: String = std::fs::read_to_string(&path)
            .unwrap()
            .lines()
            .map(|l| {
                let own = l.starts_with(&format!("{{\"id\":\"{r1}\""));
                format!("{}\n", if own { "{broken" } else { l })
            })
            .collect();
        assert!(text.contains("{broken"));
        std::fs::write(&path, text).unwrap();
        let s = Store::open(&path).unwrap();
        let texts: Vec<String> = s.prompts().into_iter().map(|p| p.text).collect();
        assert_eq!(texts, ["one", "two"]);
    }

    #[test]
    fn recent_prompts_newest_sessions_first() {
        let home = tempfile::tempdir().unwrap();
        let paths = Paths::new(home.path());
        let cwd = Path::new("/w");
        let write = |prompts: &[&str]| {
            let mut s = Store::create(&paths, cwd, false);
            for p in prompts {
                s.append(msg(p)).unwrap();
                s.append(reply("ok")).unwrap();
            }
            s.append(EntryKind::CustomMessage {
                kind: "nudge".into(),
                text: "hidden".into(),
                display: false,
            })
            .unwrap();
            (s.id().to_string(), s.path().unwrap().to_path_buf())
        };
        let (_, old) = write(&["a", "b"]);
        let (new_id, new) = write(&["c", "d"]);
        let t = SystemTime::now();
        let set = |p: &Path, secs| {
            std::fs::File::options()
                .write(true)
                .open(p)
                .unwrap()
                .set_modified(t - std::time::Duration::from_secs(secs))
                .unwrap()
        };
        set(&old, 100);
        set(&new, 10);
        // An unreadable line is skipped.
        std::fs::OpenOptions::new()
            .append(true)
            .open(&old)
            .unwrap()
            .write_all(b"{\"role\":\"user\" broken\n")
            .unwrap();
        set(&old, 100);
        assert_eq!(recent_prompts(&paths, cwd, None, 10), ["a", "b", "c", "d"]);
        assert_eq!(recent_prompts(&paths, cwd, None, 3), ["b", "c", "d"]);
        assert_eq!(recent_prompts(&paths, cwd, Some(&new_id), 10), ["a", "b"]);
        assert!(recent_prompts(&paths, Path::new("/other"), None, 10).is_empty());
        // A subagent's session in the same folder is not the user's history.
        let child = format!("{SUBAGENT_TASK_PREFIX}\"scout\" task");
        write(&[child.as_str(), "more"]);
        assert_eq!(recent_prompts(&paths, cwd, None, 10), ["a", "b", "c", "d"]);
        // More than 20 newer subagent sessions don't crowd out the user's.
        for _ in 0..21 {
            write(&[child.as_str()]);
        }
        assert_eq!(list(&paths, cwd).len(), 24);
        assert_eq!(recent_prompts(&paths, cwd, None, 10), ["a", "b", "c", "d"]);
    }

    #[test]
    fn ephemeral_never_writes() {
        let home = tempfile::tempdir().unwrap();
        let mut s = Store::create(&Paths::new(home.path()), Path::new("/w"), true);
        s.append(msg("a")).unwrap();
        s.append(reply("b")).unwrap();
        assert!(s.path().is_none() && !home.path().join("sessions").exists());
    }
}
