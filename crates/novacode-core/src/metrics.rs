//! Dev metrics (L11.11): an opt-in recorder that listens to the session's events
//! and appends one JSON line per model call, tool call, retry, notice and task to
//! `<home>/metrics/<session>.jsonl`. The loop and tools don't know it exists.

use novacode_base::redact::Redactor;
use novacode_types::{AgentEvent, Block, BlockKind, Message, StopReason, StreamEvent, Usage, estimate};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Instant;
use tokio::sync::broadcast;

/// Characters kept from tool arguments and output.
const PREVIEW: usize = 200;

/// One line of a metrics file. `ts` is epoch milliseconds when the thing started.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum Record {
    Session {
        ts: u64,
        session: String,
        cwd: String,
        version: String,
        window: u64,
    },
    /// One model call, retries included.
    Model {
        ts: u64,
        task: u32,
        turn: u32,
        provider: String,
        model: String,
        /// Time to the first streamed token.
        ttft_ms: Option<u64>,
        duration_ms: u64,
        usage: Usage,
        /// Share of the prompt read from cache, in percent.
        hit_pct: f64,
        /// Context after this reply: prompt plus output.
        ctx_tokens: u64,
        stop: String,
        retries: u32,
    },
    Tool {
        ts: u64,
        task: u32,
        turn: u32,
        name: String,
        call_id: String,
        duration_ms: u64,
        args_bytes: usize,
        args_preview: String,
        out_bytes: usize,
        out_tokens: u64,
        out_preview: String,
        is_error: bool,
    },
    Retry {
        ts: u64,
        task: u32,
        turn: u32,
        attempt: u32,
        delay_ms: u64,
        error: String,
        message: String,
    },
    /// Cache misses, warm refreshes, account switches.
    Notice { ts: u64, task: u32, text: String },
    /// One turn: a model call, then its tools, until the next turn starts.
    /// `wall_ms = model_ms + preflight_ms + tools_ms + overhead_ms`.
    Turn {
        ts: u64,
        task: u32,
        turn: u32,
        wall_ms: u64,
        model_ms: u64,
        ttft_ms: Option<u64>,
        /// Streaming time by block kind, from block start to end (the last attempt).
        thinking_ms: u64,
        text_ms: u64,
        tool_args_ms: u64,
        /// Reply to first tool start: approvals and checks.
        preflight_ms: u64,
        tools_ms: u64,
        /// Everything else: prompt build, session writes, hooks.
        overhead_ms: u64,
        tool_calls: u32,
        usage: Usage,
        hit_pct: f64,
        ctx_tokens: u64,
    },
    /// One prompt, from start to the loop stopping.
    Task {
        ts: u64,
        task: u32,
        /// Time since the previous task ended (reading, typing); `None` for the first.
        #[serde(default)]
        idle_ms: Option<u64>,
        prompt_preview: String,
        wall_ms: u64,
        /// Time inside model calls.
        model_ms: u64,
        /// Wall time while tools ran (parallel tools count once).
        tools_ms: u64,
        /// Between a reply and its tools starting: approvals and checks.
        preflight_ms: u64,
        turns: u32,
        tool_calls: u32,
        tool_errors: u32,
        retries: u32,
        usage: Usage,
        hit_pct: f64,
        ctx_tokens: u64,
        stop: String,
        /// Events the recorder missed because it fell behind.
        lagged: u64,
    },
}

pub fn path(home: &std::path::Path, session: &str) -> PathBuf {
    home.join("metrics").join(format!("{session}.jsonl"))
}

pub fn hit_pct(u: &Usage) -> f64 {
    let total = u.prompt_total();
    if total == 0 {
        0.0
    } else {
        (u.cache_read as f64 * 1000.0 / total as f64).round() / 10.0
    }
}

fn add(a: &mut Usage, b: &Usage) {
    a.input += b.input;
    a.output += b.output;
    a.cache_read += b.cache_read;
    a.cache_write += b.cache_write;
    if let Some(r) = b.reasoning {
        *a.reasoning.get_or_insert(0) += r;
    }
}

fn preview(redactor: &Redactor, text: &str) -> String {
    let text = redactor.apply(text).unwrap_or_else(|| text.to_string());
    let one_line: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut s: String = one_line.chars().take(PREVIEW).collect();
    if one_line.chars().count() > PREVIEW {
        s.push('…');
    }
    s
}

fn text_of(blocks: &[Block]) -> String {
    blocks
        .iter()
        .filter_map(|b| match b {
            Block::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn stop_name(s: &StopReason) -> String {
    match s {
        StopReason::Error { kind } => format!("error:{kind:?}"),
        s => format!("{s:?}").to_lowercase(),
    }
}

fn now_ms() -> u64 {
    crate::session::now_ms()
}

#[derive(Default)]
struct Task {
    n: u32,
    ts: u64,
    start: Option<Instant>,
    prompt: String,
    turn: u32,
    model_ms: u64,
    tools_ms: u64,
    preflight_ms: u64,
    tool_calls: u32,
    tool_errors: u32,
    retries: u32,
    usage: Usage,
    ctx: u64,
    // The current model call.
    call_start: Option<(Instant, u64)>,
    first_token: Option<u64>,
    call_retries: u32,
    // The current tool phase.
    reply_at: Option<Instant>,
    tools_start: Option<Instant>,
    running: BTreeMap<String, Running>,
    cur: Option<Turn>,
}

/// The open turn: task totals at its start, so the turn's share is a difference.
struct Turn {
    start: Instant,
    ts: u64,
    model_ms: u64,
    tools_ms: u64,
    preflight_ms: u64,
    tool_calls: u32,
    usage: Usage,
    ttft: Option<u64>,
    /// thinking, text, tool args.
    split: [u64; 3],
    blocks: BTreeMap<usize, (Instant, usize)>,
}

fn sub(a: &Usage, b: &Usage) -> Usage {
    Usage {
        input: a.input - b.input,
        output: a.output - b.output,
        cache_read: a.cache_read - b.cache_read,
        cache_write: a.cache_write - b.cache_write,
        reasoning: None,
    }
}

impl Task {
    fn open_turn(&mut self, now: Instant) {
        self.cur = Some(Turn {
            start: now,
            ts: now_ms(),
            model_ms: self.model_ms,
            tools_ms: self.tools_ms,
            preflight_ms: self.preflight_ms,
            tool_calls: self.tool_calls,
            usage: self.usage,
            ttft: None,
            split: [0; 3],
            blocks: BTreeMap::new(),
        });
    }

    fn close_turn(&mut self, now: Instant) -> Option<Record> {
        let c = self.cur.take()?;
        let wall_ms = now.duration_since(c.start).as_millis() as u64;
        let model_ms = self.model_ms - c.model_ms;
        let preflight_ms = self.preflight_ms - c.preflight_ms;
        let tools_ms = self.tools_ms - c.tools_ms;
        let usage = sub(&self.usage, &c.usage);
        Some(Record::Turn {
            ts: c.ts,
            task: self.n,
            turn: self.turn,
            wall_ms,
            model_ms,
            ttft_ms: c.ttft,
            thinking_ms: c.split[0],
            text_ms: c.split[1],
            tool_args_ms: c.split[2],
            preflight_ms,
            tools_ms,
            overhead_ms: wall_ms.saturating_sub(model_ms + preflight_ms + tools_ms),
            tool_calls: self.tool_calls - c.tool_calls,
            usage,
            hit_pct: hit_pct(&usage),
            ctx_tokens: self.ctx,
        })
    }
}

/// A started tool call.
struct Running {
    start: Instant,
    ts: u64,
    name: String,
    args_preview: String,
    args_bytes: usize,
    /// Set by `ToolEnd`.
    duration_ms: Option<u64>,
}

/// Records events until the session's event stream closes.
pub async fn record(mut rx: broadcast::Receiver<AgentEvent>, file: PathBuf, header: Record, redactor: Redactor) {
    let _ = std::fs::create_dir_all(file.parent().unwrap_or(&file));
    let write = |r: &Record| {
        if let Err(e) = novacode_base::storage::append_jsonl(&file, r) {
            tracing::warn!(%e, "metrics write");
        }
    };
    // A resumed session continues its task numbers.
    let old = load(&file);
    let mut n = old.iter().filter(|r| matches!(r, Record::Task { .. })).count() as u32;
    let mut last_end: Option<u64> = old.iter().rev().find_map(|r| match r {
        Record::Task { ts, wall_ms, .. } => Some(ts + wall_ms),
        _ => None,
    });
    if n == 0 {
        write(&header);
    }
    let mut t = Task::default();
    let mut lagged = 0;
    loop {
        let ev = match rx.recv().await {
            Ok(ev) => ev,
            Err(broadcast::error::RecvError::Lagged(k)) => {
                lagged += k;
                continue;
            }
            Err(broadcast::error::RecvError::Closed) => break,
        };
        let now = Instant::now();
        let ms = |since: Instant| now.duration_since(since).as_millis() as u64;
        match ev {
            AgentEvent::AgentStart => {
                n += 1;
                t = Task {
                    n,
                    ts: now_ms(),
                    start: Some(now),
                    ..Default::default()
                };
                lagged = 0;
            }
            AgentEvent::Message(Message::User { content }) if t.prompt.is_empty() => {
                t.prompt = preview(&redactor, &text_of(&content));
            }
            AgentEvent::TurnStart => {
                if let Some(r) = t.close_turn(now) {
                    write(&r);
                }
                t.open_turn(now);
                t.turn += 1;
                t.call_start = Some((now, now_ms()));
                t.first_token = None;
                t.call_retries = 0;
            }
            AgentEvent::Stream(
                ev @ (StreamEvent::BlockStart { .. } | StreamEvent::Delta { .. } | StreamEvent::BlockEnd { .. }),
            ) => {
                if t.first_token.is_none()
                    && let Some((s, _)) = t.call_start
                {
                    t.first_token = Some(ms(s));
                    if let Some(c) = t.cur.as_mut() {
                        c.ttft = t.first_token;
                    }
                }
                if let Some(c) = t.cur.as_mut() {
                    match ev {
                        StreamEvent::BlockStart { index, kind, .. } => {
                            let k = match kind {
                                BlockKind::Thinking => 0,
                                BlockKind::Text => 1,
                                _ => 2,
                            };
                            c.blocks.insert(index, (now, k));
                        }
                        StreamEvent::BlockEnd { index, .. } => {
                            if let Some((s, k)) = c.blocks.remove(&index) {
                                c.split[k] += ms(s);
                            }
                        }
                        _ => {}
                    }
                }
            }
            AgentEvent::Stream(StreamEvent::RetryRollback) => {
                t.first_token = None;
                if let Some(c) = t.cur.as_mut() {
                    c.split = [0; 3];
                    c.blocks.clear();
                }
            }
            AgentEvent::Stream(StreamEvent::Notice(text)) => write(&Record::Notice {
                ts: now_ms(),
                task: t.n,
                text,
            }),
            AgentEvent::Retry {
                attempt,
                delay_ms,
                error,
                message,
            } => {
                t.retries += 1;
                t.call_retries += 1;
                write(&Record::Retry {
                    ts: now_ms(),
                    task: t.n,
                    turn: t.turn,
                    attempt,
                    delay_ms,
                    error: format!("{error:?}"),
                    message,
                });
            }
            AgentEvent::Message(Message::Assistant(a)) => {
                let (start, ts) = t.call_start.take().unwrap_or((now, now_ms()));
                let duration_ms = ms(start);
                t.model_ms += duration_ms;
                add(&mut t.usage, &a.usage);
                if a.usage.prompt_total() > 0 {
                    t.ctx = a.usage.prompt_total() + a.usage.output;
                }
                t.reply_at = Some(now);
                write(&Record::Model {
                    ts,
                    task: t.n,
                    turn: t.turn,
                    provider: a.provider.clone(),
                    model: a.model.clone(),
                    ttft_ms: t.first_token,
                    duration_ms,
                    usage: a.usage,
                    hit_pct: hit_pct(&a.usage),
                    ctx_tokens: t.ctx,
                    stop: stop_name(&a.stop),
                    retries: t.call_retries,
                });
            }
            AgentEvent::ToolStart { call } => {
                if let Some(r) = t.reply_at.take() {
                    t.preflight_ms += ms(r);
                }
                t.tools_start.get_or_insert(now);
                let args = call
                    .arguments_error
                    .clone()
                    .unwrap_or_else(|| call.arguments.to_string());
                t.running.insert(
                    call.id.clone(),
                    Running {
                        start: now,
                        ts: now_ms(),
                        name: call.name.clone(),
                        args_preview: preview(&redactor, &args),
                        args_bytes: args.len(),
                        duration_ms: None,
                    },
                );
            }
            // The tool's own time: results of parallel tools arrive together, later.
            AgentEvent::ToolEnd {
                call_id, duration_ms, ..
            } => {
                if let Some(r) = t.running.get_mut(&call_id) {
                    r.duration_ms = Some(duration_ms);
                }
            }
            AgentEvent::Message(Message::ToolResult {
                call_id,
                tool_name,
                content,
                is_error,
                ..
            }) => {
                let out = text_of(&content);
                // Refused calls never start: they count as calls, with no time.
                let (duration_ms, ts, name, args_preview, args_bytes) = match t.running.remove(&call_id) {
                    Some(r) => (
                        r.duration_ms.unwrap_or_else(|| ms(r.start)),
                        r.ts,
                        r.name,
                        r.args_preview,
                        r.args_bytes,
                    ),
                    None => (0, now_ms(), tool_name, String::new(), 0),
                };
                t.tool_calls += 1;
                t.tool_errors += is_error as u32;
                write(&Record::Tool {
                    ts,
                    task: t.n,
                    turn: t.turn,
                    name,
                    call_id,
                    duration_ms,
                    args_bytes,
                    args_preview,
                    out_bytes: out.len(),
                    out_tokens: estimate::message(&Message::User {
                        content: content.clone(),
                    }),
                    out_preview: preview(&redactor, &out),
                    is_error,
                });
                if t.running.is_empty()
                    && let Some(s) = t.tools_start.take()
                {
                    t.tools_ms += ms(s);
                }
            }
            AgentEvent::AgentEnd { stop } => {
                if let Some(r) = t.close_turn(now) {
                    write(&r);
                }
                let Some(start) = t.start.take() else { continue };
                let idle_ms = last_end.map(|e| t.ts.saturating_sub(e));
                last_end = Some(now_ms());
                write(&Record::Task {
                    ts: t.ts,
                    task: t.n,
                    idle_ms,
                    prompt_preview: std::mem::take(&mut t.prompt),
                    wall_ms: ms(start),
                    model_ms: t.model_ms,
                    tools_ms: t.tools_ms,
                    preflight_ms: t.preflight_ms,
                    turns: t.turn,
                    tool_calls: t.tool_calls,
                    tool_errors: t.tool_errors,
                    retries: t.retries,
                    usage: t.usage,
                    hit_pct: hit_pct(&t.usage),
                    ctx_tokens: t.ctx,
                    stop: stop_name(&stop),
                    lagged,
                });
            }
            _ => {}
        }
    }
}

/// Read a metrics file; unreadable lines are skipped.
pub fn load(file: &std::path::Path) -> Vec<Record> {
    std::fs::read_to_string(file)
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}
