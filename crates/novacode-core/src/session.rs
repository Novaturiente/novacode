//! Session store (L4.1, L4.2, L4.4): an append-only JSONL tree, one file per
//! session. Entries are written once with single-write appends; a torn last
//! line is skipped on load. The file is created lazily at the first real
//! exchange. The projection is a pure function of the file (cache rule L1.7).

use novacode_base::storage::{self, FileLock};
use novacode_base::{new_id, paths::Paths};
use novacode_types::{Block, Message};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
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
    /// Moves the leaf (branch navigation). Not a tree node.
    Leaf {
        target: String,
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
            _lock: None,
        }
    }

    pub fn open(path: &Path) -> Result<Self, StoreError> {
        let lock = storage::try_lock(path)?.ok_or_else(|| StoreError::Busy(path.display().to_string()))?;
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
            _lock: Some(lock),
        };
        for (n, line) in lines.enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<Entry>(line) {
                Ok(e) => s.push(e),
                Err(e) => {
                    tracing::warn!(path = %path.display(), line = n + 2, error = %e, "skipping unreadable session line")
                }
            }
        }
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
        if let Some(path) = self.path.clone() {
            if self.on_disk {
                storage::append_jsonl(&path, self.entries.last().expect("pushed"))?;
            } else if first_exchange {
                self.materialise(&path)?;
            }
        }
        Ok(id)
    }

    /// Write the header and everything so far, creating the file (lazy creation).
    fn materialise(&mut self, path: &Path) -> Result<(), StoreError> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let lock = storage::try_lock(path)?.ok_or_else(|| StoreError::Busy(self.header.id.clone()))?;
        let mut buf = serde_json::to_string(&self.header).map_err(io::Error::other)?;
        buf.push('\n');
        for e in &self.entries {
            buf.push_str(&serde_json::to_string(e).map_err(io::Error::other)?);
            buf.push('\n');
        }
        let mut f = std::fs::OpenOptions::new().write(true).create_new(true).open(path)?;
        f.write_all(buf.as_bytes())?;
        self.on_disk = true;
        self._lock = Some(lock);
        Ok(())
    }

    /// Entries from the root to the leaf.
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
        let path = self.path_to_leaf();
        let mut edits: HashMap<&str, Option<&Message>> = HashMap::new();
        for e in &path {
            if let EntryKind::ContextEdit { target, replace } = &e.kind {
                edits.insert(target, replace.as_ref());
            }
        }
        let mut out = Vec::new();
        for e in path {
            let msg = match &e.kind {
                EntryKind::Message { message } => message.clone(),
                EntryKind::CustomMessage { text, .. } => Message::User {
                    content: vec![Block::text(format!("<system-reminder>\n{text}\n</system-reminder>"))],
                },
                _ => continue,
            };
            match edits.get(e.id.as_str()) {
                Some(None) => {}
                Some(Some(r)) => out.push((*r).clone()),
                None => out.push(msg),
            }
        }
        out
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

    /// Last recorded model, as (provider, model).
    pub fn last_model(&self) -> Option<(String, String)> {
        self.entries.iter().rev().find_map(|e| match &e.kind {
            EntryKind::ModelChange { provider, model } => Some((provider.clone(), model.clone())),
            _ => None,
        })
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
                        Block::Text { text, .. } => Some(text.lines().next().unwrap_or("").to_string()),
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
pub fn find(paths: &Paths, cwd: &Path, prefix: &str) -> Result<PathBuf, StoreError> {
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
    fn ephemeral_never_writes() {
        let home = tempfile::tempdir().unwrap();
        let mut s = Store::create(&Paths::new(home.path()), Path::new("/w"), true);
        s.append(msg("a")).unwrap();
        s.append(reply("b")).unwrap();
        assert!(s.path().is_none() && !home.path().join("sessions").exists());
    }
}
