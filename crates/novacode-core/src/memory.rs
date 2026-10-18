//! Session memory (L4.5): an observer writes short notes about the conversation
//! as it goes, so compaction only has to render them, with no LLM call.
//!
//! Notes are session entries (`EntryKind::Notes`), so they follow branches and
//! survive restarts. They are never sent to the main model between compactions.

use crate::session::{Entry, EntryKind};
use novacode_ai::adapter::{Adapter, CacheRetention, Request};
use novacode_types::{Block, Message, StreamEvent, ToolDef, Usage, estimate};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::fmt::Write;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

/// How much matters if a note is lost.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Relevance {
    Low,
    Medium,
    High,
    Critical,
}

/// One remembered fact (claude-mem's types plus pi-om's relevance).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Note {
    /// 12 hex chars, from the content.
    pub id: String,
    pub content: String,
    pub relevance: Relevance,
    /// decision, bugfix, feature, refactor, change, discovery, preference.
    #[serde(rename = "type")]
    pub kind: String,
    /// Entry ids the note is based on, for `recall`.
    pub sources: Vec<String>,
    /// Time of the earliest source, epoch ms.
    pub ts_ms: u64,
}

/// The model that writes notes.
#[derive(Clone)]
pub struct Worker {
    pub adapter: Arc<dyn Adapter>,
    pub provider: String,
    pub model: String,
}

/// Memory settings (`memory.*`).
#[derive(Debug, Clone)]
pub struct Settings {
    /// New conversation tokens before the observer runs.
    pub observe_after: u64,
    /// Tokens kept verbatim at compaction.
    pub keep_recent: u64,
    /// Compact once the context reaches this many tokens.
    pub compact_at: u64,
    /// Largest chunk sent to the observer in one call.
    pub chunk_tokens: u64,
}

impl Settings {
    pub fn for_window(window: u64) -> Self {
        Self {
            observe_after: 10_000,
            keep_recent: 20_000,
            compact_at: (window * 8 / 10).min(400_000),
            chunk_tokens: 30_000,
        }
    }
}

/// Conversation entries prepared for the observer.
#[derive(Debug, Clone, Default)]
pub struct Chunk {
    pub text: String,
    /// `#n` in the text → entry id.
    pub aliases: Vec<String>,
    /// Entry ids' times, same order as `aliases`.
    pub times: Vec<u64>,
    /// Id of the last entry covered.
    pub covers: String,
    pub tokens: u64,
}

const RESULT_CHARS: usize = 1500;
const TEXT_CHARS: usize = 6000;
const ARG_CHARS: usize = 300;

fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let head: String = s.chars().take(max * 2 / 3).collect();
    let tail: Vec<char> = s.chars().rev().take(max / 3).collect();
    let tail: String = tail.into_iter().rev().collect();
    format!("{head}\n[… cut …]\n{tail}")
}

fn blocks_text(blocks: &[Block], max: usize) -> String {
    let mut s = String::new();
    for b in blocks {
        match b {
            Block::Text { text, .. } => s.push_str(text),
            Block::Image { .. } => s.push_str("[image]"),
            _ => {}
        }
    }
    clip(s.trim(), max)
}

/// The text the observer sees for one entry; `None` for entries it skips.
pub fn render_entry(e: &Entry) -> Option<String> {
    render_capped(e, TEXT_CHARS, RESULT_CHARS)
}

fn render_capped(e: &Entry, text_chars: usize, result_chars: usize) -> Option<String> {
    match &e.kind {
        EntryKind::Message { message } => match message {
            Message::User { content } => Some(format!("user: {}", blocks_text(content, text_chars))),
            Message::Assistant(a) => {
                let mut s = String::from("assistant:");
                let text = blocks_text(&a.content, text_chars);
                if !text.is_empty() {
                    let _ = write!(s, " {text}");
                }
                for c in a.tool_calls() {
                    let _ = write!(s, "\n→ {}({})", c.name, clip(&c.arguments.to_string(), ARG_CHARS));
                }
                (s != "assistant:").then_some(s)
            }
            Message::ToolResult {
                tool_name,
                content,
                is_error,
                ..
            } => Some(format!(
                "result of {tool_name}{}: {}",
                if *is_error { " (error)" } else { "" },
                blocks_text(content, result_chars)
            )),
            _ => None,
        },
        EntryKind::CustomMessage {
            text, display: true, ..
        } => Some(format!("note shown to user: {}", clip(text, text_chars))),
        _ => None,
    }
}

/// Index in `path` after the last entry the observer covered.
pub fn watermark(path: &[&Entry]) -> usize {
    let pos = |id: &str| path.iter().position(|e| e.id == id);
    path.iter()
        .rev()
        .find_map(|e| match &e.kind {
            EntryKind::Notes { covers, .. } => pos(covers),
            _ => None,
        })
        .map_or(0, |i| i + 1)
}

/// Estimated conversation tokens in `path[from..to]`.
pub fn tokens(path: &[&Entry], from: usize, to: usize) -> u64 {
    path[from.min(to)..to]
        .iter()
        .map(|e| match &e.kind {
            EntryKind::Message { message } => estimate::message(message),
            EntryKind::CustomMessage { text, .. } => estimate::text(text),
            _ => 0,
        })
        .sum()
}

/// Entries from `from` up to (not including) `to`, until `budget` tokens.
pub fn chunk(path: &[&Entry], from: usize, to: usize, budget: u64) -> Chunk {
    let mut c = Chunk::default();
    for e in &path[from.min(to)..to] {
        if c.tokens >= budget && !c.covers.is_empty() {
            break;
        }
        c.covers = e.id.clone();
        let Some(text) = render_entry(e) else { continue };
        c.aliases.push(e.id.clone());
        c.times.push(e.ts_ms);
        let _ = write!(c.text, "[#{}] {text}\n\n", c.aliases.len());
        c.tokens += estimate::text(&text);
    }
    c
}

pub const OBSERVER_PROMPT: &str = "You are the memory observer for a coding agent. You read a chunk of the agent's conversation and record short notes with the record_notes tool. Once the raw conversation is compacted away, your notes are all the agent will remember of it: what you leave out is forgotten, what you distort is remembered wrong.

You get the notes already recorded, then a new chunk. Each chunk entry starts with [#n]. Record notes for the new chunk only, without repeating existing notes unless something changed.

Each note:
- content: one line of plain prose, no markdown. One fact per note: split compound statements.
- Keep the details that can't be re-derived: full file paths with line numbers, identifiers, commands, exact error messages, numbers with units, versions, commit hashes.
- Record what the user TOLD you as an assertion (\"User stated the API must stay backward compatible\"), and questions as questions.
- Frame changes as supersession (\"Will use sqlx, replacing diesel\"), and mark finished work with \"completed:\" so it isn't redone.
- Group repeated similar tool calls into one note (\"Surveyed src/auth/*.rs; token check is in src/auth/jwt.rs:45\").
- Skip routine noise. Recording nothing is fine when a chunk has nothing new.
- type: decision, bugfix, feature, refactor, change, discovery or preference.
- relevance: critical (user preferences, corrections, completions that must not be redone), high (technical decisions, blockers, constraints), medium (task context, the default), low (routine). Most notes are medium or low.
- sources: the [#n] numbers the note is based on. Notes without valid sources are rejected.

Call record_notes once with every note, or reply with plain text and no call when there is nothing to record.";

fn record_tool() -> ToolDef {
    ToolDef {
        name: "record_notes".into(),
        description: "Record notes about the conversation chunk.".into(),
        schema: json!({
            "type": "object",
            "properties": {
                "notes": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "content": {"type": "string"},
                            "type": {"type": "string", "enum": ["decision", "bugfix", "feature", "refactor", "change", "discovery", "preference"]},
                            "relevance": {"type": "string", "enum": ["low", "medium", "high", "critical"]},
                            "sources": {"type": "array", "items": {"type": "integer"}}
                        },
                        "required": ["content", "type", "relevance", "sources"]
                    }
                }
            },
            "required": ["notes"]
        }),
    }
}

/// A stable id from the content, so the same fact can't be recorded twice.
pub fn note_id(content: &str) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in content.bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    format!("{h:016x}")[..12].to_string()
}

/// Notes from the observer's tool arguments; invalid ones are dropped.
pub fn parse_notes(args: &Value, c: &Chunk, known: &[Note]) -> Vec<Note> {
    let mut out: Vec<Note> = Vec::new();
    for n in args["notes"].as_array().into_iter().flatten() {
        let content = n["content"]
            .as_str()
            .unwrap_or_default()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        let Ok(relevance) = serde_json::from_value::<Relevance>(n["relevance"].clone()) else {
            continue;
        };
        let idx: Vec<usize> = n["sources"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|v| v.as_u64().or_else(|| v.as_str()?.trim_start_matches('#').parse().ok()))
            .filter_map(|i| (i as usize).checked_sub(1).filter(|&i| i < c.aliases.len()))
            .collect();
        if content.is_empty() || idx.is_empty() {
            continue;
        }
        let id = note_id(&content);
        if known.iter().chain(&out).any(|k| k.id == id) {
            continue;
        }
        out.push(Note {
            id,
            content: content.chars().take(2000).collect(),
            relevance,
            kind: n["type"].as_str().unwrap_or("change").to_string(),
            sources: idx.iter().map(|&i| c.aliases[i].clone()).collect(),
            ts_ms: idx.iter().map(|&i| c.times[i]).min().unwrap_or_default(),
        });
    }
    out
}

/// One observer call. The system prompt is fixed and existing notes only grow
/// at the end between calls, so the worker's own prefix stays cacheable.
pub async fn observe(w: &Worker, known: &[Note], c: &Chunk, session: &str) -> Result<(Vec<Note>, Usage), String> {
    let mut existing = String::from("Notes already recorded:\n");
    if known.is_empty() {
        existing.push_str("(none)\n");
    }
    for n in known {
        let _ = writeln!(existing, "- {}", n.content);
    }
    let req = Request {
        model: w.model.clone(),
        messages: vec![
            Message::System {
                text: OBSERVER_PROMPT.into(),
            },
            Message::User {
                content: vec![Block::text(existing), Block::text(format!("New chunk:\n\n{}", c.text))],
            },
        ],
        tools: vec![record_tool()],
        max_tokens: 8192,
        cache: CacheRetention::Short,
        thinking: None,
        images: false,
        session: Some(format!("{session}-memory")),
    };
    let mut rx = w.adapter.stream(req, CancellationToken::new());
    while let Some(ev) = rx.recv().await {
        match ev {
            StreamEvent::Done(m) => {
                let mut notes = Vec::new();
                for call in m.tool_calls().filter(|t| t.name == "record_notes") {
                    let mut all = known.to_vec();
                    all.extend(notes.iter().cloned());
                    notes.extend(parse_notes(&call.arguments, c, &all));
                }
                return Ok((notes, m.usage));
            }
            StreamEvent::Error(m) => return Err(m.error_message.unwrap_or_else(|| format!("{:?}", m.stop))),
            _ => {}
        }
    }
    Err("observer stream ended without a result".into())
}

/// Every note on the branch, oldest first.
pub fn notes(path: &[&Entry]) -> Vec<Note> {
    path.iter()
        .filter_map(|e| match &e.kind {
            EntryKind::Notes { notes, .. } => Some(notes.iter().cloned()),
            _ => None,
        })
        .flatten()
        .collect()
}

/// `YYYY-MM-DD HH:MM` UTC.
pub fn utc_minute(ms: u64) -> String {
    let secs = (ms / 1000) as i64;
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02} {:02}:{:02}", rem / 3600, rem % 3600 / 60)
}

pub fn note_line(n: &Note) -> String {
    format!(
        "[{}] {}Z [{:?}] {}: {}",
        n.id,
        utc_minute(n.ts_ms),
        n.relevance,
        n.kind,
        n.content
    )
    .replace("[Low]", "[low]")
    .replace("[Medium]", "[medium]")
    .replace("[High]", "[high]")
    .replace("[Critical]", "[critical]")
}

/// Files the compacted part read and changed, from its tool calls.
pub fn files(path: &[&Entry]) -> (Vec<String>, Vec<String>) {
    let (mut read, mut changed) = (Vec::<String>::new(), Vec::<String>::new());
    for e in path {
        let EntryKind::Message {
            message: Message::Assistant(a),
        } = &e.kind
        else {
            continue;
        };
        for c in a.tool_calls() {
            let target = match c.name.as_str() {
                "read" => &mut read,
                "edit" | "write" => &mut changed,
                _ => continue,
            };
            if let Some(p) = c.arguments["path"].as_str()
                && !target.iter().any(|x| x == p)
            {
                target.push(p.to_string());
            }
        }
    }
    read.retain(|p| !changed.contains(p));
    (read, changed)
}

pub const SUMMARY_INTRO: &str = "The conversation before this point was compacted into the notes below.

Treat them as past records. When notes conflict, the newer one holds. Work a note marks as completed should not be redone unless the user asks. When you need the exact source behind a note, call the recall tool with its id; don't use recall as a search.";

/// The text the model gets in place of the compacted conversation.
pub fn render_summary(notes: &[Note], read: &[String], changed: &[String], dropped_messages: usize) -> String {
    let mut s = String::from(SUMMARY_INTRO);
    if notes.is_empty() {
        let _ = write!(
            s,
            "\n\nNo notes were available: {dropped_messages} earlier messages were removed without a summary. Ask the user if you need what they held."
        );
    } else {
        s.push_str("\n\n## Notes\n");
        for n in notes {
            s.push_str(&note_line(n));
            s.push('\n');
        }
    }
    if !changed.is_empty() {
        let _ = write!(s, "\n## Files changed\n{}\n", changed.join("\n"));
    }
    if !read.is_empty() {
        let _ = write!(s, "\n## Files read\n{}\n", read.join("\n"));
    }
    s.trim_end().to_string()
}

/// Where to cut: the start of the earliest user message such that at least
/// `keep` tokens of conversation come after it. `None` when nothing would be cut.
pub fn cut_point(path: &[&Entry], from: usize, keep: u64) -> Option<usize> {
    let mut kept = 0;
    let mut i = path.len();
    while i > from {
        i -= 1;
        kept += tokens(path, i, i + 1);
        if kept >= keep {
            break;
        }
    }
    // Move back to a user message, so a tool call is never split from its result.
    let is_user = |e: &Entry| {
        matches!(
            &e.kind,
            EntryKind::Message {
                message: Message::User { .. }
            }
        )
    };
    while i > from && !is_user(path[i]) {
        i -= 1;
    }
    (i > from && is_user(path[i])).then_some(i)
}

/// `recall(id)`: a note and the raw entries it came from (L4.5). Reads the
/// session file, so it sees notes stored since the session started.
pub struct Recall {
    pub file: std::path::PathBuf,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
struct RecallArgs {
    /// The 12-character note id shown in brackets in the compaction summary.
    id: String,
}

impl novacode_tools::Tool for Recall {
    fn name(&self) -> &str {
        "recall"
    }
    fn description(&self) -> &str {
        "Get the exact source behind a note from the compaction summary: the note and the original messages it was written from. Takes one note id; not a search."
    }
    fn schema(&self) -> Value {
        novacode_tools::schema_for::<RecallArgs>()
    }
    fn guidelines(&self) -> &[&str] {
        &["Use `recall` with a note id when a compacted note matters and you need its exact wording, paths or errors."]
    }
    fn execute<'a>(
        &'a self,
        args: Value,
        _ctx: &'a novacode_tools::ToolCtx,
    ) -> futures_util::future::BoxFuture<'a, novacode_tools::ToolOutput> {
        Box::pin(async move {
            let a: RecallArgs = match novacode_tools::parse(args) {
                Ok(a) => a,
                Err(e) => return e,
            };
            let text = std::fs::read_to_string(&self.file).unwrap_or_default();
            let entries: Vec<Entry> = text
                .lines()
                .skip(1)
                .filter_map(|l| serde_json::from_str(l).ok())
                .collect();
            let id = a.id.trim().trim_matches(['[', ']']);
            let Some(note) = entries
                .iter()
                .filter_map(|e| match &e.kind {
                    EntryKind::Notes { notes, .. } => Some(notes),
                    _ => None,
                })
                .flatten()
                .find(|n| n.id == id)
            else {
                return novacode_tools::ToolOutput::error(format!("no note with id `{id}` in this session"));
            };
            let mut s = format!("{}\n\nSources:", note_line(note));
            for src in &note.sources {
                match entries
                    .iter()
                    .find(|e| &e.id == src)
                    .and_then(|e| render_capped(e, 20_000, 8_000))
                {
                    Some(t) => {
                        let _ = write!(
                            s,
                            "\n\n[{}] {t}",
                            utc_minute(entries.iter().find(|e| &e.id == src).map_or(0, |e| e.ts_ms))
                        );
                    }
                    None => {
                        let _ = write!(s, "\n\n(source {src} is missing)");
                    }
                }
            }
            novacode_tools::ToolOutput::text(s)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str, kind: EntryKind) -> Entry {
        Entry {
            id: id.into(),
            parent: None,
            ts_ms: 1_760_000_000_000,
            kind,
        }
    }
    fn user(id: &str, t: &str) -> Entry {
        entry(
            id,
            EntryKind::Message {
                message: Message::User {
                    content: vec![Block::text(t)],
                },
            },
        )
    }

    #[test]
    fn chunk_parse_and_cut() {
        let es = [user("a", "use sqlx"), user("b", &"x".repeat(400)), user("c", "ok")];
        let path: Vec<&Entry> = es.iter().collect();
        let c = chunk(&path, 0, 3, 1_000_000);
        assert!(c.text.starts_with("[#1] user: use sqlx"));
        assert_eq!((c.covers.as_str(), c.aliases.len()), ("c", 3));
        let args = json!({"notes": [
            {"content": "User chose  sqlx", "type": "decision", "relevance": "critical", "sources": [1]},
            {"content": "no sources", "type": "change", "relevance": "low", "sources": [9]},
            {"content": "bad relevance", "type": "change", "relevance": "huge", "sources": [1]},
        ]});
        let n = parse_notes(&args, &c, &[]);
        assert_eq!(n.len(), 1);
        assert_eq!(
            (n[0].content.as_str(), n[0].sources.as_slice()),
            ("User chose sqlx", &["a".to_string()][..])
        );
        assert!(
            parse_notes(&args, &c, &n).is_empty(),
            "known notes aren't recorded twice"
        );
        assert!(note_line(&n[0]).ends_with("[critical] decision: User chose sqlx"));
        // Keep the last ~100 tokens: the cut lands on a user message.
        assert_eq!(cut_point(&path, 0, 50), Some(1));
        assert_eq!(cut_point(&path, 0, 1_000_000), None);
        assert_eq!(utc_minute(0), "1970-01-01 00:00");
    }
}
