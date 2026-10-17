//! read (L5.4): text with offset/limit and a continuation hint; images as image blocks.

use crate::{Tool, ToolCtx, ToolOutput, guard, parse, schema_for};
use base64::Engine;
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
const MAX_IMAGE: u64 = 5 * 1024 * 1024;

fn image_mime(bytes: &[u8]) -> Option<&'static str> {
    match bytes {
        [0x89, b'P', b'N', b'G', ..] => Some("image/png"),
        [0xFF, 0xD8, 0xFF, ..] => Some("image/jpeg"),
        [b'G', b'I', b'F', b'8', ..] => Some("image/gif"),
        [b'R', b'I', b'F', b'F', _, _, _, _, b'W', b'E', b'B', b'P', ..] => Some("image/webp"),
        _ => None,
    }
}

impl Tool for Read {
    fn name(&self) -> &str {
        "read"
    }
    fn description(&self) -> &str {
        "Read a file. Text output is cut at 2000 lines or 50KB; use offset/limit to page. Images (png, jpeg, gif, webp) are returned as images."
    }
    fn schema(&self) -> Value {
        schema_for::<Args>()
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
            let bytes = match ctx.env.read(&path).await {
                Ok(b) => b,
                Err(e) => return ToolOutput::error(format!("cannot read {}: {e}", a.path)),
            };
            let real = ctx.env.canonical(&path).await;
            ctx.files.saw(&real, meta.mtime_ms);
            // shortcut: images are sent as-is (size-capped); normalise/resize with L2.22 in M5.
            if let Some(mime) = image_mime(&bytes) {
                if meta.len > MAX_IMAGE {
                    return ToolOutput::error(format!("image is {} bytes; the limit is {MAX_IMAGE}", meta.len));
                }
                let data = base64::engine::general_purpose::STANDARD.encode(&bytes);
                return ToolOutput {
                    content: vec![
                        Block::text(format!("Image {}:", a.path)),
                        Block::Image {
                            mime_type: mime.into(),
                            data,
                        },
                    ],
                    is_error: false,
                    details: None,
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
            let limit = a.limit.unwrap_or(guard::MAX_LINES).min(guard::MAX_LINES);
            let mut out = String::new();
            let mut end = start;
            for l in lines.iter().skip(start).take(limit) {
                let l = match l.char_indices().nth(MAX_LINE) {
                    Some((i, _)) => format!("{}… [line clipped]", &l[..i]),
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
            ToolOutput::text(out)
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
}
