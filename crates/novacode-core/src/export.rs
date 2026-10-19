//! Session export (L4.14) and the `session_search` tool (L5.19).

use crate::session::{Entry, EntryKind, Header};
use novacode_base::redact::Redactor;
use novacode_types::{Block, Message};
use serde_json::Value;
use std::fmt::Write;
use std::path::{Path, PathBuf};

/// Header and entries of a session file, read without locking it.
pub fn read(path: &Path) -> Option<(Header, Vec<Entry>)> {
    let text = std::fs::read_to_string(path).ok()?;
    let mut lines = text.lines();
    let header: Header = serde_json::from_str(lines.next()?).ok()?;
    let entries = lines.filter_map(|l| serde_json::from_str(l).ok()).collect();
    Some((header, entries))
}

/// The branch ending at the file's leaf, root first.
pub fn branch(entries: &[Entry]) -> Vec<&Entry> {
    let mut leaf = None;
    for e in entries {
        leaf = Some(match &e.kind {
            EntryKind::Leaf { target } => target.as_str(),
            _ => e.id.as_str(),
        });
    }
    let by_id: std::collections::HashMap<&str, &Entry> = entries.iter().map(|e| (e.id.as_str(), e)).collect();
    let mut out = Vec::new();
    let mut cur = leaf;
    while let Some(id) = cur {
        let Some(e) = by_id.get(id) else { break };
        out.push(*e);
        cur = e.parent.as_deref();
    }
    out.reverse();
    out
}

fn redact(r: &Redactor, s: &str) -> String {
    r.apply(s).unwrap_or_else(|| s.to_string())
}

/// The branch's messages, one JSON object per line, secrets masked.
pub fn jsonl(entries: &[Entry], r: &Redactor) -> String {
    let mut out = String::new();
    for e in branch(entries) {
        if let EntryKind::Message { message } = &e.kind {
            let mut v = serde_json::to_value(message).unwrap_or(Value::Null);
            v["ts_ms"] = e.ts_ms.into();
            out.push_str(&redact(r, &v.to_string()));
            out.push('\n');
        }
    }
    out
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn blocks_text(blocks: &[Block]) -> String {
    blocks
        .iter()
        .filter_map(|b| match b {
            Block::Text { text, .. } => Some(text.clone()),
            Block::Image { .. } => Some("[image]".into()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

const CSS: &str = "body{font:15px/1.5 system-ui,sans-serif;max-width:900px;margin:2em auto;padding:0 1em;background:#1e1e2e;color:#cdd6f4}
.m{margin:1em 0;padding:.6em .9em;border-radius:8px;white-space:pre-wrap;word-wrap:break-word}
.user{background:#313244;border-left:4px solid #89b4fa}.assistant{background:#181825}
.tool{background:#11111b;font:13px/1.4 ui-monospace,monospace;color:#a6adc8;max-height:24em;overflow:auto}
.err{border-left:4px solid #f38ba8}.call{color:#f9e2af;font-family:ui-monospace,monospace;font-size:13px}
.think{color:#7f849c;font-style:italic}.meta{color:#7f849c;font-size:12px}h1{font-size:18px}";

/// A self-contained HTML page of the branch, secrets masked.
pub fn html(header: &Header, entries: &[Entry], r: &Redactor) -> String {
    let mut s = format!(
        "<!doctype html><meta charset=utf-8><title>novacode session {id}</title><style>{CSS}</style>\n<h1>novacode session {id}</h1><div class=meta>{cwd}</div>\n",
        id = esc(&header.id),
        cwd = esc(&header.cwd.display().to_string())
    );
    for e in branch(entries) {
        let EntryKind::Message { message } = &e.kind else {
            continue;
        };
        let div = |class: &str, body: &str| format!("<div class=\"m {class}\">{}</div>\n", esc(&redact(r, body)));
        match message {
            Message::User { content } => s.push_str(&div("user", &blocks_text(content))),
            Message::Assistant(a) => {
                for b in &a.content {
                    match b {
                        Block::Thinking { text, .. } if !text.trim().is_empty() => {
                            s.push_str(&div("assistant think", text))
                        }
                        Block::Text { text, .. } if !text.trim().is_empty() => s.push_str(&div("assistant", text)),
                        Block::ToolCall(c) => {
                            let _ = writeln!(
                                s,
                                "<div class=call>→ {}({})</div>",
                                esc(&c.name),
                                esc(&redact(r, &c.arguments.to_string()))
                            );
                        }
                        _ => {}
                    }
                }
            }
            Message::ToolResult { content, is_error, .. } => {
                s.push_str(&div(if *is_error { "tool err" } else { "tool" }, &blocks_text(content)))
            }
            _ => {}
        }
    }
    s
}

/// One hit of `session_search`.
pub struct Hit {
    pub session: String,
    pub title: String,
    pub ts_ms: u64,
    pub role: &'static str,
    pub snippet: String,
}

fn text_of(m: &Message) -> Option<(&'static str, String)> {
    Some(match m {
        Message::User { content } => ("user", blocks_text(content)),
        Message::Assistant(a) => {
            let mut t = blocks_text(&a.content);
            for c in a.tool_calls() {
                let _ = write!(t, "\n→ {}({})", c.name, c.arguments);
            }
            ("assistant", t)
        }
        Message::ToolResult { content, tool_name, .. } => ("tool", format!("{tool_name}: {}", blocks_text(content))),
        _ => return None,
    })
}

/// Search session files for messages holding every word of `query`
/// (case-insensitive), newest sessions first. No index: a plain scan (L5.19).
pub fn search(files: &[PathBuf], query: &str, skip: Option<&str>, limit: usize) -> Vec<Hit> {
    let words: Vec<String> = query.split_whitespace().map(str::to_lowercase).collect();
    if words.is_empty() {
        return vec![];
    }
    let mut hits = Vec::new();
    for f in files {
        let Ok(text) = std::fs::read_to_string(f) else { continue };
        // Cheap prefilter on the raw file before parsing anything.
        let lower = text.to_lowercase();
        if !words.iter().all(|w| lower.contains(w.as_str())) {
            continue;
        }
        let mut lines = text.lines();
        let Some(header) = lines.next().and_then(|l| serde_json::from_str::<Header>(l).ok()) else {
            continue;
        };
        if Some(header.id.as_str()) == skip {
            continue;
        }
        let mut title = String::new();
        for l in lines {
            let lw = l.to_lowercase();
            let maybe = words.iter().any(|w| lw.contains(w.as_str()));
            let Ok(e) = serde_json::from_str::<Entry>(l) else {
                continue;
            };
            let EntryKind::Message { message } = &e.kind else {
                continue;
            };
            if title.is_empty()
                && let Message::User { content } = message
            {
                title = blocks_text(content)
                    .lines()
                    .next()
                    .unwrap_or("")
                    .chars()
                    .take(80)
                    .collect();
            }
            if !maybe {
                continue;
            }
            let Some((role, t)) = text_of(message) else { continue };
            let tl = t.to_lowercase();
            if !words.iter().all(|w| tl.contains(w.as_str())) {
                continue;
            }
            let at = tl.find(words[0].as_str()).unwrap_or(0);
            let start = (0..=at.saturating_sub(120))
                .rev()
                .find(|&i| t.is_char_boundary(i))
                .unwrap_or(0);
            let snippet: String = t[start..]
                .chars()
                .take(320)
                .collect::<String>()
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
            hits.push(Hit {
                session: header.id.clone(),
                title: title.clone(),
                ts_ms: e.ts_ms,
                role,
                snippet,
            });
            if hits.len() >= limit {
                return hits;
            }
        }
    }
    hits
}

/// `session_search`: find earlier sessions' messages about something (L5.19).
pub struct SessionSearch {
    /// Session folders: this project's first, then the rest when `everywhere`.
    pub sessions_root: PathBuf,
    pub project_dir: PathBuf,
    pub current: String,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
struct SearchArgs {
    /// Words that must all appear in a message (case-insensitive).
    query: String,
    /// Search every project's sessions, not only this folder's.
    #[serde(default)]
    all_projects: bool,
    /// Most hits to return (default 20).
    #[serde(default)]
    limit: Option<usize>,
}

fn newest_first(dir: &Path) -> Vec<PathBuf> {
    let mut v: Vec<(std::time::SystemTime, PathBuf)> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "jsonl"))
        .map(|e| {
            (
                e.metadata().and_then(|m| m.modified()).unwrap_or(std::time::UNIX_EPOCH),
                e.path(),
            )
        })
        .collect();
    v.sort_by_key(|(t, _)| std::cmp::Reverse(*t));
    v.into_iter().map(|(_, p)| p).collect()
}

impl novacode_tools::Tool for SessionSearch {
    fn name(&self) -> &str {
        "session_search"
    }
    /// Rarely needed: found through tool_search, so it costs no tokens per call.
    fn exposure(&self) -> novacode_tools::Exposure {
        novacode_tools::Exposure::Deferred
    }
    fn description(&self) -> &str {
        "Search earlier novacode sessions (this folder's, or all projects') for messages containing all the given words. Returns matching snippets with session ids and dates."
    }
    fn schema(&self) -> Value {
        novacode_tools::schema_for::<SearchArgs>()
    }
    fn execute<'a>(
        &'a self,
        args: Value,
        _ctx: &'a novacode_tools::ToolCtx,
    ) -> futures_util::future::BoxFuture<'a, novacode_tools::ToolOutput> {
        Box::pin(async move {
            let a: SearchArgs = match novacode_tools::parse(args) {
                Ok(a) => a,
                Err(e) => return e,
            };
            let mut files = newest_first(&self.project_dir);
            if a.all_projects {
                for d in std::fs::read_dir(&self.sessions_root).into_iter().flatten().flatten() {
                    if d.path() != self.project_dir {
                        files.extend(newest_first(&d.path()));
                    }
                }
            }
            let (q, cur, limit) = (
                a.query.clone(),
                self.current.clone(),
                a.limit.unwrap_or(20).clamp(1, 100),
            );
            let hits = tokio::task::spawn_blocking(move || search(&files, &q, Some(&cur), limit))
                .await
                .unwrap_or_default();
            if hits.is_empty() {
                return novacode_tools::ToolOutput::text(format!(
                    "no messages in earlier sessions contain all of: {}",
                    a.query
                ));
            }
            let mut s = String::new();
            let mut last = String::new();
            for h in &hits {
                if h.session != last {
                    let _ = write!(
                        s,
                        "\n## session {} — {}\n",
                        &h.session[..h.session.len().min(13)],
                        h.title
                    );
                    last = h.session.clone();
                }
                let _ = writeln!(s, "[{}Z {}] {}", crate::memory::utc_minute(h.ts_ms), h.role, h.snippet);
            }
            novacode_tools::ToolOutput::text(s.trim_start().to_string())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::Store;
    use novacode_base::paths::Paths;

    #[test]
    fn export_and_search() {
        let home = tempfile::tempdir().unwrap();
        let paths = Paths::new(home.path());
        let mut s = Store::create(&paths, home.path(), false);
        s.append(EntryKind::Message {
            message: Message::user("how do I fix the <Postgres> pool timeout? key sk-secret-123456"),
        })
        .unwrap();
        s.append(EntryKind::Message {
            message: Message::Assistant(novacode_types::AssistantMessage {
                content: vec![Block::text("Raise pool_timeout in db.toml")],
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
        let (h, entries) = read(&file).unwrap();
        let r = Redactor::new(|| vec![("sk-secret-123456".to_string(), "test key".to_string())]);
        let page = html(&h, &entries, &r);
        assert!(page.contains("&lt;Postgres&gt;") && page.contains("[redacted: test key]"));
        assert!(!page.contains("sk-secret"));
        let j = jsonl(&entries, &r);
        assert_eq!(j.lines().count(), 2);
        assert!(!j.contains("sk-secret"));
        let hits = search(std::slice::from_ref(&file), "POOL timeout", None, 10);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].role, "user");
        assert!(search(std::slice::from_ref(&file), "pool mysql", None, 10).is_empty());
        assert!(
            search(&[file], "pool", Some(s.id()), 10).is_empty(),
            "the current session is skipped"
        );
    }
}
