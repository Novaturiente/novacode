//! read (L5.4): text with offset/limit and a continuation hint; images as image blocks.

use crate::{Tool, ToolCtx, ToolOutput, guard, parse, schema_for};
use futures_util::future::BoxFuture;
use novacode_types::Block;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

/// Read delegation (L5.4b): the provider's small model reads a file for the session's
/// model. Implemented in core (it needs a provider); `model` names it in the output.
pub trait Delegate: Send + Sync {
    fn model(&self) -> String;
    /// The small model's answer to `user` under the instructions `system`.
    fn ask<'a>(
        &'a self,
        system: &'static str,
        user: String,
        cancel: CancellationToken,
    ) -> BoxFuture<'a, Result<String, String>>;
}

/// `read`; with a [`Delegate`], a `question` is answered by the small model and a full
/// read of a file over `above_lines` lines returns the small model's map of it instead.
#[derive(Default, Clone)]
pub struct Read {
    pub delegate: Option<Arc<dyn Delegate>>,
    /// 0: large full reads are never mapped.
    pub above_lines: usize,
}

#[derive(Deserialize, JsonSchema)]
struct Args {
    /// File path, relative to the working directory or absolute.
    path: String,
    /// First line to read, 1-based.
    offset: Option<usize>,
    /// Maximum number of lines to read.
    limit: Option<usize>,
    /// Ask a small model about the file (or the offset/limit range) instead of reading it:
    /// the answer cites file:line and the file's text stays out of your context. Not for
    /// editing (read the range), debugging or design decisions.
    #[serde(default)]
    question: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
struct PlainArgs {
    /// File path, relative to the working directory or absolute.
    #[allow(dead_code)]
    path: String,
    /// First line to read, 1-based.
    #[allow(dead_code)]
    offset: Option<usize>,
    /// Maximum number of lines to read.
    #[allow(dead_code)]
    limit: Option<usize>,
}

/// Characters of numbered file text one delegated call takes.
const DELEGATE_CHARS: usize = 400_000;

const QUESTION_PROMPT: &str = "You read a file for a coding agent and answer its question about it. Answer only what \
was asked, in short bullets. Start each bullet with the file path and line number(s) as given (`src/a.rs:120-134`), \
then the fact: names, types, values and behaviour exactly as in the file; quote short code exactly when it matters. \
If the file doesn't answer the question, say so in one line. No greeting, no summary of the whole file.";

const MAP_PROMPT: &str = "You map a file for a coding agent that will read only the parts it needs. List the file's \
sections in order, one per line, as `start-end: what is there` (functions, types, config blocks, headings; names \
exact). At most 40 lines: group small neighbours. No other text.";

const DESCRIPTION: &str = "Read a file. Text output is cut at 2000 lines or 50KB; use offset/limit to page. Images are returned as images (converted and shrunk to fit when needed).";
const DESCRIPTION_DELEGATE: &str = "Read a file. Text output is cut at 2000 lines or 50KB; use offset/limit to page. Images are returned as images (converted and shrunk to fit when needed). A full read of a large text file returns a map of its sections with line ranges instead: read the range you need. `question` asks a small model about the file instead of reading it (answers cite file:line; read those lines before editing).";

/// `lines` from 1-based line `first`, numbered, at most [`DELEGATE_CHARS`]; and the last line included.
fn numbered(lines: &[&str], first: usize) -> (String, usize) {
    let mut s = String::new();
    let mut last = first.saturating_sub(1);
    for (i, l) in lines.iter().enumerate() {
        let l = match l.char_indices().nth(MAX_LINE) {
            Some((b, _)) => &l[..b],
            None => l,
        };
        if s.len() + l.len() + 8 > DELEGATE_CHARS {
            break;
        }
        s.push_str(&format!("{}\t{l}\n", first + i));
        last = first + i;
    }
    (s, last)
}

const MAX_LINE: usize = 2000;
/// Files above this are refused before loading (images are capped lower, in `image`).
const MAX_FILE: u64 = 64 << 20;
impl Read {
    /// A question answered, or a large full read mapped, by the small model. `None`: read
    /// normally (nothing to delegate, or the small model failed: a read still works).
    async fn delegated(
        &self,
        d: &dyn Delegate,
        a: &Args,
        lines: &[&str],
        start: usize,
        ctx: &ToolCtx,
    ) -> Option<ToolOutput> {
        let total = lines.len();
        let question = a.question.as_deref().map(str::trim).filter(|q| !q.is_empty());
        let full = a.offset.is_none() && a.limit.is_none();
        if question.is_none() && !(full && self.above_lines > 0 && total > self.above_lines) {
            return None;
        }
        let end = a.limit.map_or(total, |l| (start + l).min(total));
        let (text, last) = numbered(&lines[start..end], start + 1);
        let file = format!("<file path=\"{}\">\n{text}</file>", a.path);
        let cut = match last < end {
            true => format!(" (only lines {}-{last} fit)", start + 1),
            false => String::new(),
        };
        let (system, user) = match question {
            Some(q) => (QUESTION_PROMPT, format!("{file}\n\nQuestion: {q}")),
            None => (MAP_PROMPT, file),
        };
        let reply = match d.ask(system, user, ctx.cancel.clone()).await {
            Ok(r) if !r.trim().is_empty() => r,
            Ok(_) => return None,
            Err(e) => {
                tracing::warn!("read delegation: {e}");
                return None;
            }
        };
        let model = d.model();
        Some(ToolOutput::text(match question {
            Some(_) => format!(
                "[{model} read {} lines {}-{last} of {total}{cut} and answered; read the cited lines before editing.]\n{}",
                a.path,
                start + 1,
                reply.trim()
            ),
            None => format!(
                "{} has {total} lines, too many to show at once. Map by {model}{cut}:\n{}\n\nRead the range you need with offset/limit (edits need the exact text), or pass `question` to ask about the file.",
                a.path,
                reply.trim()
            ),
        }))
    }
}

impl Tool for Read {
    fn name(&self) -> &str {
        "read"
    }
    fn description(&self) -> &str {
        match self.delegate {
            Some(_) => DESCRIPTION_DELEGATE,
            None => DESCRIPTION,
        }
    }
    fn schema(&self) -> Value {
        match self.delegate {
            Some(_) => schema_for::<Args>(),
            None => schema_for::<PlainArgs>(),
        }
    }
    fn guidelines(&self) -> &[&str] {
        &[
            "Use `read` to look at files, not cat, head, tail or sed in bash: it pages with offset/limit instead of cutting the output.",
        ]
    }
    fn execute<'a>(&'a self, args: Value, ctx: &'a ToolCtx) -> BoxFuture<'a, ToolOutput> {
        Box::pin(async move {
            let a: Args = match parse(args) {
                Ok(a) => a,
                Err(e) => return e,
            };
            let path = ctx.path(&a.path);
            let meta = match ctx.env.stat(&path).await {
                Ok(Some(m)) if m.is_dir => return ToolOutput::error(format!("{} is a directory; use ls", a.path)),
                Ok(Some(m)) => m,
                Ok(None) => return ToolOutput::error(format!("{} does not exist", a.path)),
                Err(e) => return ToolOutput::error(format!("cannot read {}: {e}", a.path)),
            };
            if meta.len > MAX_FILE {
                return ToolOutput::error(format!(
                    "{} is too large to read ({} MiB, limit {} MiB); use grep, or bash `sed -n 'a,bp' file`",
                    a.path,
                    meta.len >> 20,
                    MAX_FILE >> 20
                ));
            }
            let bytes = match ctx.env.read(&path).await {
                Ok(b) => b,
                Err(e) => return ToolOutput::error(format!("cannot read {}: {e}", a.path)),
            };
            let real = ctx.env.canonical(&path).await;
            ctx.files.saw(&real, meta.mtime_ms);
            if crate::image::is_image(&bytes) {
                return match crate::image::normalize(&bytes) {
                    Ok(img) => ToolOutput {
                        content: vec![
                            Block::text(format!("Image {} ({}):", a.path, img.describe())),
                            img.block(),
                        ],
                        is_error: false,
                        details: None,
                    },
                    Err(e) => ToolOutput::error(format!("{}: {e}", a.path)),
                };
            }
            if bytes.iter().take(8000).any(|&b| b == 0) {
                return ToolOutput::error(format!("{} looks binary ({} bytes); not shown", a.path, meta.len));
            }
            let text = String::from_utf8_lossy(&bytes);
            let lines: Vec<&str> = text.lines().collect();
            let total = lines.len();
            let start = a.offset.unwrap_or(1).max(1) - 1;
            if start >= total && total > 0 {
                return ToolOutput::error(format!("offset {} is past the end ({total} lines)", start + 1));
            }
            // Read delegation (L5.4b): never for a script, which wants the text itself.
            if let Some(d) = self.delegate.as_ref().filter(|_| !ctx.script)
                && let Some(out) = self.delegated(d.as_ref(), &a, &lines, start, ctx).await
            {
                return out;
            }
            // A script gets the file as it is: no clipped lines, paging or footer.
            if ctx.script {
                let s = lines.iter().skip(start).take(a.limit.unwrap_or(usize::MAX));
                return ToolOutput::text(s.copied().collect::<Vec<_>>().join("\n"));
            }
            let limit = a.limit.unwrap_or(guard::MAX_LINES).min(guard::MAX_LINES);
            let mut out = String::new();
            let mut end = start;
            for l in lines.iter().skip(start).take(limit) {
                let l = match l.char_indices().nth(MAX_LINE) {
                    Some((i, _)) => format!(
                        "{}… [line clipped at {MAX_LINE} chars; use grep or bash `cut -c` to see the rest]",
                        &l[..i]
                    ),
                    None => l.to_string(),
                };
                if out.len() + l.len() + 1 > guard::MAX_BYTES {
                    break;
                }
                out.push_str(&l);
                out.push('\n');
                end += 1;
            }
            if end < total {
                out.push_str(&format!(
                    "\n[Lines {}-{end} of {total}. Continue with offset={}.]",
                    start + 1,
                    end + 1
                ));
            }
            // The file's fingerprint: an identical re-read can be answered with a pointer (agent).
            let sha = {
                use std::hash::{Hash, Hasher};
                let mut h = std::collections::hash_map::DefaultHasher::new();
                bytes.hash(&mut h);
                format!("{:016x}", h.finish())
            };
            ToolOutput {
                details: Some(serde_json::json!({ "sha": sha })),
                ..ToolOutput::text(out)
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{ctx, text};
    use serde_json::json;

    #[tokio::test]
    async fn pages_and_errors() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(
            d.path().join("a.txt"),
            (1..=10).map(|i| format!("l{i}\n")).collect::<String>(),
        )
        .unwrap();
        let c = ctx(d.path());
        let o = Read::default()
            .execute(json!({"path": "a.txt", "offset": 3, "limit": 2}), &c)
            .await;
        assert_eq!(text(&o), "l3\nl4\n\n[Lines 3-4 of 10. Continue with offset=5.]");
        let o = Read::default().execute(json!({"path": "a.txt"}), &c).await;
        assert!(text(&o).ends_with("l10\n"));
        assert!(Read::default().execute(json!({"path": "nope"}), &c).await.is_error);
        assert!(Read::default().execute(json!({"path": "."}), &c).await.is_error);
    }

    /// A small model that records what it was asked and answers `reply` (or fails).
    struct Fake {
        reply: Result<String, String>,
        asked: std::sync::Mutex<Vec<(&'static str, String)>>,
    }

    impl Delegate for Fake {
        fn model(&self) -> String {
            "p/small".into()
        }
        fn ask<'a>(
            &'a self,
            system: &'static str,
            user: String,
            _: CancellationToken,
        ) -> BoxFuture<'a, Result<String, String>> {
            self.asked.lock().unwrap().push((system, user));
            Box::pin(async move { self.reply.clone() })
        }
    }

    fn delegating(reply: Result<&str, &str>) -> (Read, Arc<Fake>) {
        let fake = Arc::new(Fake {
            reply: reply.map(String::from).map_err(String::from),
            asked: Default::default(),
        });
        let read = Read {
            delegate: Some(fake.clone()),
            above_lines: 400,
        };
        (read, fake)
    }

    #[tokio::test]
    async fn delegation_answers_questions_and_maps_large_full_reads_only() {
        let d = tempfile::tempdir().unwrap();
        let big: String = (1..=401).map(|i| format!("line {i}\n")).collect();
        std::fs::write(d.path().join("big.rs"), &big).unwrap();
        std::fs::write(d.path().join("small.rs"), "a\nb\n").unwrap();
        let c = ctx(d.path());
        let (read, fake) = delegating(Ok("1-401: lines"));
        assert!(read.schema().to_string().contains("question"));
        assert!(!Read::default().schema().to_string().contains("question"));
        // A full read past the threshold: the map, not the text.
        let o = read.execute(json!({"path": "big.rs"}), &c).await;
        assert!(
            text(&o).contains("has 401 lines") && text(&o).contains("Map by p/small") && !text(&o).contains("line 7\n"),
            "{}",
            text(&o)
        );
        let (system, user) = fake.asked.lock().unwrap()[0].clone();
        assert_eq!(system, MAP_PROMPT);
        assert!(user.contains("<file path=\"big.rs\">\n1\tline 1\n") && user.contains("401\tline 401\n"));
        // Ranged, small, script: read as before, no model call.
        let o = read
            .execute(json!({"path": "big.rs", "offset": 5, "limit": 2}), &c)
            .await;
        assert!(text(&o).starts_with("line 5\nline 6\n"), "{}", text(&o));
        assert!(text(&read.execute(json!({"path": "small.rs"}), &c).await).starts_with("a\nb"));
        let mut sc = ctx(d.path());
        sc.script = true;
        assert!(text(&read.execute(json!({"path": "big.rs"}), &sc).await).starts_with("line 1\n"));
        assert_eq!(fake.asked.lock().unwrap().len(), 1);
        // A question, on a range: only those lines go to the small model.
        let o = read
            .execute(json!({"path": "small.rs", "offset": 2, "question": "what is b?"}), &c)
            .await;
        assert!(
            text(&o).starts_with("[p/small read small.rs lines 2-2 of 2"),
            "{}",
            text(&o)
        );
        let (system, user) = fake.asked.lock().unwrap()[1].clone();
        assert_eq!(system, QUESTION_PROMPT);
        assert!(
            user.contains("2\tb\n</file>\n\nQuestion: what is b?") && !user.contains("1\ta"),
            "{user}"
        );
        // A failed small model: a normal read.
        let (read, _) = delegating(Err("down"));
        assert!(text(&read.execute(json!({"path": "big.rs"}), &c).await).starts_with("line 1\n"));
    }

    #[tokio::test]
    async fn huge_file_refused_and_long_line_clipped() {
        let d = tempfile::tempdir().unwrap();
        let f = std::fs::File::create(d.path().join("big")).unwrap();
        f.set_len(100 << 20).unwrap();
        std::fs::write(d.path().join("long"), "x".repeat(5000)).unwrap();
        let c = ctx(d.path());
        let o = Read::default().execute(json!({"path": "big"}), &c).await;
        assert!(o.is_error && text(&o).contains("too large"), "{}", text(&o));
        let o = Read::default().execute(json!({"path": "long"}), &c).await;
        assert!(
            text(&o).contains("… [line clipped at 2000 chars; use grep or bash `cut -c` to see the rest]"),
            "{}",
            text(&o)
        );
    }
}
