//! Session memory (L4.5): an observer writes short notes about the conversation
//! as it goes, so compaction only has to render them, with no LLM call.
//!
//! Notes are session entries (`EntryKind::Notes`), so they follow branches and
//! survive restarts. They are never sent to the main model between compactions.

use crate::session::{Entry, EntryKind};
use novacode_ai::adapter::{Adapter, CacheRetention, Request};
use novacode_types::{Block, Message, StopReason, StreamEvent, ToolDef, Usage, estimate};
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
    /// New conversation tokens before the reflector runs.
    pub reflect_after: u64,
    /// Active notes the dropper prunes back to, in tokens.
    pub pool_target: u64,
}

impl Settings {
    pub fn for_window(window: u64) -> Self {
        Self {
            observe_after: 10_000,
            keep_recent: 20_000,
            compact_at: (window * 8 / 10).min(400_000),
            chunk_tokens: 30_000,
            reflect_after: 20_000,
            pool_target: 10_000,
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
const CUT_OFF: &str = "reply hit the output token limit";

pub async fn observe(w: &Worker, known: &[Note], c: &Chunk, session: &str) -> Result<(Vec<Note>, Usage), String> {
    let mut existing = String::from("Notes already recorded:\n");
    if known.is_empty() {
        existing.push_str("(none)\n");
    }
    for n in known {
        let _ = writeln!(existing, "- {}", n.content);
    }
    let req = Request {
        tail: Vec::new(),
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
            // A cut-off record_notes call parses as `{}`: fail so the chunk isn't marked covered.
            StreamEvent::Done(m) if m.stop == StopReason::Length => return Err(CUT_OFF.into()),
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
pub fn render_summary(
    refl: &[Reflection],
    notes: &[Note],
    fallback: Option<&str>,
    read: &[String],
    changed: &[String],
    dropped_messages: usize,
) -> String {
    let mut s = String::from(SUMMARY_INTRO);
    if !refl.is_empty() {
        s.push_str("\n\n## Lasting facts\n");
        for r in refl {
            let _ = writeln!(s, "[{}] {}", r.id, r.content);
        }
        s.pop();
    }
    if let Some(f) = fallback {
        let _ = write!(s, "\n\n## Summary of the earlier conversation\n{}", f.trim());
    }
    if notes.is_empty() && fallback.is_none() && refl.is_empty() {
        let _ = write!(
            s,
            "\n\nNo notes were available: {dropped_messages} earlier messages were removed without a summary. Ask the user if you need what they held."
        );
    } else if !notes.is_empty() {
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

/// Where to cut (pi's rule): the first user or assistant message at or after the
/// point where `keep` tokens of conversation remain, so a long turn can be split.
/// Tool results are never a cut: they stay with the call before them. `None` when
/// nothing would be cut.
pub fn cut_point(path: &[&Entry], from: usize, keep: u64) -> Option<usize> {
    let mut kept = 0;
    let mut i = path.len();
    loop {
        if i <= from {
            return None;
        }
        i -= 1;
        kept += tokens(path, i, i + 1);
        if kept >= keep {
            break;
        }
    }
    let valid = |e: &Entry| {
        matches!(
            &e.kind,
            EntryKind::Message {
                message: Message::User { .. } | Message::Assistant(_)
            }
        )
    };
    // Trailing tool results bigger than `keep`: keep their call too.
    let cut = (i..path.len())
        .find(|&j| valid(path[j]))
        .or_else(|| (from + 1..i).rev().find(|&j| valid(path[j])))?;
    (cut > from).then_some(cut)
}

/// The prompt of the turn a cut splits, word for word; `None` when the cut is at
/// a user message. shortcut: a steering message sent mid-turn counts as the prompt;
/// collect the turn's earlier user messages too if that loses requests.
pub fn split_prompt(path: &[&Entry], cut: usize) -> Option<String> {
    let user = |e: &Entry| match &e.kind {
        EntryKind::Message {
            message: Message::User { content },
        } => Some(content.clone()),
        _ => None,
    };
    if user(path[cut]).is_some() {
        return None;
    }
    let content = path[..cut].iter().rev().find_map(|e| user(e))?;
    let text: Vec<String> = content
        .into_iter()
        .filter_map(|b| match b {
            Block::Text { text, .. } => Some(text),
            _ => None,
        })
        .collect();
    (!text.is_empty()).then(|| text.join("\n"))
}

/// A lasting fact distilled from several notes by the reflector.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Reflection {
    pub id: String,
    pub content: String,
    /// Note ids whose meaning this reflection preserves.
    pub supports: Vec<String>,
}

pub const REFLECTOR_PROMPT: &str = "You are the reflector for a coding agent's memory. You get the lasting facts already distilled (reflections) and the active notes. Distil NEW lasting facts with the record_reflections tool: user preferences, constraints and corrections; decisions and their reasons; finished outcomes that must not be redone; long-lived blockers; project goals and invariants.

- Be stingy. Recording nothing is fine. Don't turn each note into a reflection; combine notes into one durable conclusion, or keep a single note's durable user assertion, decision or completion.
- Leave working state, files inspected, commands run and failed attempts as notes.
- Don't reword existing reflections.
- content: one line of plain prose, leading with the fact, keeping exact names, paths and reasons.
- supports: ids of the notes whose meaning the reflection fully keeps. Only those: a later step may drop the notes you list.";

pub const DROPPER_PROMPT: &str = "You prune a coding agent's memory. You get the reflections and the active notes, each note marked with how many reflections cite it. Propose notes to drop with the drop_notes tool. The default is KEEP; when unsure, keep.

Drop, in this order: notes whose meaning a reflection already keeps; notes a newer note supersedes; routine low-signal notes. Prefer older notes. Never drop a note that alone carries a user preference, correction or constraint, a completion, an exact identifier, path, command or error message, a decision, a date, or an open blocker. Stay under the maximum given; it is a limit, not a target.";

fn reflect_tool() -> ToolDef {
    ToolDef {
        name: "record_reflections".into(),
        description: "Record new lasting facts.".into(),
        schema: json!({
            "type": "object",
            "properties": {"reflections": {"type": "array", "items": {"type": "object", "properties": {
                "content": {"type": "string"},
                "supports": {"type": "array", "items": {"type": "string"}}
            }, "required": ["content", "supports"]}}},
            "required": ["reflections"]
        }),
    }
}

fn drop_tool() -> ToolDef {
    ToolDef {
        name: "drop_notes".into(),
        description: "Drop notes from active memory.".into(),
        schema: json!({
            "type": "object",
            "properties": {"ids": {"type": "array", "items": {"type": "string"}}},
            "required": ["ids"]
        }),
    }
}

/// All reflections on the branch.
pub fn reflections(path: &[&Entry]) -> Vec<Reflection> {
    path.iter()
        .filter_map(|e| match &e.kind {
            EntryKind::Reflections { reflections, .. } => Some(reflections.iter().cloned()),
            _ => None,
        })
        .flatten()
        .collect()
}

/// Notes not dropped, oldest first.
pub fn active_notes(path: &[&Entry]) -> Vec<Note> {
    let dropped: std::collections::HashSet<&String> = path
        .iter()
        .filter_map(|e| match &e.kind {
            EntryKind::Drops { ids, .. } => Some(ids.iter()),
            _ => None,
        })
        .flatten()
        .collect();
    notes(path).into_iter().filter(|n| !dropped.contains(&n.id)).collect()
}

/// Index in `path` after the conversation the reflector last looked at.
pub fn reflect_mark(path: &[&Entry]) -> usize {
    let pos = |id: &str| path.iter().position(|e| e.id == id);
    path.iter()
        .rev()
        .find_map(|e| match &e.kind {
            EntryKind::Reflections { covers, .. } => pos(covers),
            _ => None,
        })
        .map_or(0, |i| i + 1)
}

pub fn pool_tokens(notes: &[Note]) -> u64 {
    notes.iter().map(|n| estimate::text(&note_line(n))).sum()
}

async fn ask(
    w: &Worker,
    system: &str,
    user: String,
    tool: ToolDef,
    session: &str,
) -> Result<(Vec<Value>, Usage), String> {
    let name = tool.name.clone();
    let req = Request {
        tail: Vec::new(),
        model: w.model.clone(),
        messages: vec![
            Message::System { text: system.into() },
            Message::User {
                content: vec![Block::text(user)],
            },
        ],
        tools: vec![tool],
        max_tokens: 8192,
        cache: CacheRetention::Short,
        thinking: None,
        images: false,
        session: Some(format!("{session}-memory")),
    };
    let mut rx = w.adapter.stream(req, CancellationToken::new());
    while let Some(ev) = rx.recv().await {
        match ev {
            StreamEvent::Done(m) if m.stop == StopReason::Length => return Err(CUT_OFF.into()),
            StreamEvent::Done(m) => {
                let args = m
                    .tool_calls()
                    .filter(|t| t.name == name)
                    .map(|t| t.arguments.clone())
                    .collect();
                return Ok((args, m.usage));
            }
            StreamEvent::Error(m) => return Err(m.error_message.unwrap_or_else(|| format!("{:?}", m.stop))),
            _ => {}
        }
    }
    Err("memory worker stream ended without a result".into())
}

fn cites(n: &Note, refl: &[Reflection]) -> usize {
    refl.iter().filter(|r| r.supports.contains(&n.id)).count()
}

fn pool_text(refl: &[Reflection], notes: &[Note]) -> String {
    let mut s = String::from("Reflections:\n");
    if refl.is_empty() {
        s.push_str("(none)\n");
    }
    for r in refl {
        let _ = writeln!(s, "[{}] {}", r.id, r.content);
    }
    s.push_str("\nActive notes:\n");
    for n in notes {
        let _ = writeln!(s, "{} [cited by {}]", note_line(n), cites(n, refl));
    }
    s
}

/// New reflections; supports must name active notes.
pub async fn reflect(
    w: &Worker,
    refl: &[Reflection],
    notes: &[Note],
    session: &str,
) -> Result<(Vec<Reflection>, Usage), String> {
    let (calls, usage) = ask(w, REFLECTOR_PROMPT, pool_text(refl, notes), reflect_tool(), session).await?;
    let mut out: Vec<Reflection> = Vec::new();
    for args in calls {
        for r in args["reflections"].as_array().into_iter().flatten() {
            let content = r["content"]
                .as_str()
                .unwrap_or_default()
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
            let supports: Vec<String> = r["supports"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|v| v.as_str())
                .map(|v| v.trim_matches(['[', ']']).to_string())
                .filter(|id| notes.iter().any(|n| &n.id == id))
                .collect();
            let id = note_id(&content);
            if content.is_empty() || supports.is_empty() || refl.iter().chain(&out).any(|x| x.id == id) {
                continue;
            }
            out.push(Reflection { id, content, supports });
        }
    }
    Ok((out, usage))
}

/// Notes to drop. The model proposes; code caps the count so the pool only
/// goes back to `target`, preferring notes most cited, least relevant, oldest.
pub async fn drop_notes(
    w: &Worker,
    refl: &[Reflection],
    notes: &[Note],
    target: u64,
    session: &str,
) -> Result<(Vec<String>, Usage), String> {
    let total = pool_tokens(notes);
    let avg = (total / notes.len().max(1) as u64).max(1);
    let max = total.saturating_sub(target).div_ceil(avg) as usize;
    if max == 0 {
        return Ok((vec![], Usage::default()));
    }
    let user = format!(
        "{}\nThe active notes hold about {total} tokens; the target is {target}. Maximum drops allowed: {max}.",
        pool_text(refl, notes)
    );
    let (calls, usage) = ask(w, DROPPER_PROMPT, user, drop_tool(), session).await?;
    let mut picked: Vec<&Note> = calls
        .iter()
        .flat_map(|a| a["ids"].as_array().cloned().unwrap_or_default())
        .filter_map(|v| v.as_str().map(|s| s.trim_matches(['[', ']']).to_string()))
        .filter_map(|id| notes.iter().find(|n| n.id == id))
        .collect();
    picked.sort_by_key(|n| (std::cmp::Reverse(cites(n, refl)), n.relevance, n.ts_ms));
    picked.dedup_by_key(|n| n.id.clone());
    picked.truncate(max);
    Ok((picked.into_iter().map(|n| n.id.clone()).collect(), usage))
}

pub const SUMMARY_PROMPT: &str = "Summarise this part of a coding agent's conversation so the agent can continue the work without it. Keep: the user's goals, preferences and corrections; decisions and their reasons; what was completed (so it isn't redone); open problems and next steps; exact file paths, identifiers, commands and error messages. Plain text with short sections, no preamble.";

/// Fallback summary by the session's main model, when there are no notes.
pub async fn summarize(
    adapter: &Arc<dyn Adapter>,
    model: &str,
    c: &Chunk,
    session: &str,
) -> Result<(String, Usage), String> {
    let req = Request {
        tail: Vec::new(),
        model: model.into(),
        messages: vec![
            Message::System {
                text: SUMMARY_PROMPT.into(),
            },
            Message::User {
                content: vec![Block::text(c.text.clone())],
            },
        ],
        tools: vec![],
        max_tokens: 8192,
        cache: CacheRetention::Short,
        thinking: None,
        images: false,
        session: Some(format!("{session}-summary")),
    };
    let mut rx = adapter.stream(req, CancellationToken::new());
    while let Some(ev) = rx.recv().await {
        match ev {
            StreamEvent::Done(m) if !m.text().trim().is_empty() => return Ok((m.text(), m.usage)),
            StreamEvent::Done(_) => return Err("empty summary".into()),
            StreamEvent::Error(m) => return Err(m.error_message.unwrap_or_else(|| format!("{:?}", m.stop))),
            _ => {}
        }
    }
    Err("summary stream ended without a result".into())
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
            let path: Vec<&Entry> = entries.iter().collect();
            if let Some(r) = reflections(&path).into_iter().find(|r| r.id == id) {
                let all = notes(&path);
                let mut s = format!("[{}] {}\n\nBased on notes:", r.id, r.content);
                for n in all.iter().filter(|n| r.supports.contains(&n.id)) {
                    let _ = write!(s, "\n{}", note_line(n));
                }
                s.push_str("\n\nRecall a note id for its source messages.");
                return novacode_tools::ToolOutput::text(s);
            }
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

    #[tokio::test]
    async fn cut_off_observer_reply_is_an_error() {
        use novacode_ai::faux::{Faux, FauxReply};
        let cut = FauxReply::Message {
            blocks: vec![Block::text("thinking out loud")],
            stop: StopReason::Length,
            usage: Usage::default(),
        };
        let w = Worker {
            adapter: Arc::new(Faux::new([cut])),
            provider: "faux".into(),
            model: "m".into(),
        };
        let es = [user("a", "use sqlx")];
        let path: Vec<&Entry> = es.iter().collect();
        let c = chunk(&path, 0, 1, 1_000_000);
        assert_eq!(observe(&w, &[], &c, "s").await.unwrap_err(), CUT_OFF);
    }
}
