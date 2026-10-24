//! read (L5.4): text with offset/limit and a continuation hint; images as image blocks.

use crate::{Tool, ToolCtx, ToolOutput, guard, parse, schema_for};
use futures_util::future::BoxFuture;
use novacode_types::Block;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

pub struct Read;

#[derive(Deserialize, JsonSchema)]
struct Args {
    /// File path, relative to the working directory or absolute.
    path: String,
    /// First line to read, 1-based.
    offset: Option<usize>,
    /// Maximum number of lines to read.
    limit: Option<usize>,
}

const MAX_LINE: usize = 2000;
/// Files above this are refused before loading (images are capped lower, in `image`).
const MAX_FILE: u64 = 64 << 20;
impl Tool for Read {
    fn name(&self) -> &str {
        "read"
    }
    fn description(&self) -> &str {
        "Read a file. Text output is cut at 2000 lines or 50KB; use offset/limit to page. Images are returned as images (converted and shrunk to fit when needed)."
    }
    fn schema(&self) -> Value {
        schema_for::<Args>()
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
        let o = Read
            .execute(json!({"path": "a.txt", "offset": 3, "limit": 2}), &c)
            .await;
        assert_eq!(text(&o), "l3\nl4\n\n[Lines 3-4 of 10. Continue with offset=5.]");
        let o = Read.execute(json!({"path": "a.txt"}), &c).await;
        assert!(text(&o).ends_with("l10\n"));
        assert!(Read.execute(json!({"path": "nope"}), &c).await.is_error);
        assert!(Read.execute(json!({"path": "."}), &c).await.is_error);
    }

    #[tokio::test]
    async fn huge_file_refused_and_long_line_clipped() {
        let d = tempfile::tempdir().unwrap();
        let f = std::fs::File::create(d.path().join("big")).unwrap();
        f.set_len(100 << 20).unwrap();
        std::fs::write(d.path().join("long"), "x".repeat(5000)).unwrap();
        let c = ctx(d.path());
        let o = Read.execute(json!({"path": "big"}), &c).await;
        assert!(o.is_error && text(&o).contains("too large"), "{}", text(&o));
        let o = Read.execute(json!({"path": "long"}), &c).await;
        assert!(
            text(&o).contains("… [line clipped at 2000 chars; use grep or bash `cut -c` to see the rest]"),
            "{}",
            text(&o)
        );
    }
}
