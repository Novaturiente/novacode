//! apply_patch (L5.7): the Codex patch format, for OpenAI/Codex model families.
//! All files are computed first, then written, so a bad hunk changes nothing.

use crate::{Tool, ToolCtx, ToolOutput, parse, schema_for};
use futures_util::future::BoxFuture;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

pub struct ApplyPatch;

#[derive(Deserialize, JsonSchema)]
struct Args {
    /// The patch, from `*** Begin Patch` to `*** End Patch`.
    input: String,
}

#[derive(Debug, PartialEq)]
enum Op {
    Add {
        path: String,
        content: String,
    },
    Delete {
        path: String,
    },
    Update {
        path: String,
        move_to: Option<String>,
        hunks: Vec<Hunk>,
    },
}

#[derive(Debug, PartialEq, Default)]
struct Hunk {
    header: String,
    old: Vec<String>,
    new: Vec<String>,
}

fn parse_patch(input: &str) -> Result<Vec<Op>, String> {
    let lines: Vec<&str> = input.trim().lines().collect();
    if lines.first().map(|l| l.trim()) != Some("*** Begin Patch")
        || lines.last().map(|l| l.trim()) != Some("*** End Patch")
    {
        return Err("patch must start with `*** Begin Patch` and end with `*** End Patch`".into());
    }
    let mut ops = Vec::new();
    let mut i = 1;
    let body = &lines[..lines.len() - 1];
    while i < body.len() {
        let l = body[i];
        if let Some(p) = l.strip_prefix("*** Add File: ") {
            i += 1;
            let mut content = String::new();
            while i < body.len() && !body[i].starts_with("*** ") {
                let Some(t) = body[i].strip_prefix('+') else {
                    return Err(format!("line {}: Add File lines must start with +", i + 1));
                };
                content.push_str(t);
                content.push('\n');
                i += 1;
            }
            ops.push(Op::Add {
                path: p.trim().into(),
                content,
            });
        } else if let Some(p) = l.strip_prefix("*** Delete File: ") {
            ops.push(Op::Delete { path: p.trim().into() });
            i += 1;
        } else if let Some(p) = l.strip_prefix("*** Update File: ") {
            i += 1;
            let mut move_to = None;
            if let Some(m) = body.get(i).and_then(|l| l.strip_prefix("*** Move to: ")) {
                move_to = Some(m.trim().to_string());
                i += 1;
            }
            let mut hunks: Vec<Hunk> = Vec::new();
            while i < body.len() && (!body[i].starts_with("*** ") || body[i] == "*** End of File") {
                let l = body[i];
                i += 1;
                if l == "*** End of File" {
                    continue;
                }
                if let Some(h) = l.strip_prefix("@@") {
                    hunks.push(Hunk {
                        header: h.trim().into(),
                        ..Default::default()
                    });
                    continue;
                }
                if hunks.is_empty() {
                    hunks.push(Hunk::default());
                }
                let h = hunks.last_mut().expect("pushed");
                match l.chars().next() {
                    Some('+') => h.new.push(l[1..].into()),
                    Some('-') => h.old.push(l[1..].into()),
                    Some(' ') => {
                        h.old.push(l[1..].into());
                        h.new.push(l[1..].into());
                    }
                    None => {
                        h.old.push(String::new());
                        h.new.push(String::new());
                    }
                    _ => return Err(format!("line {i}: hunk lines must start with ' ', '+' or '-': {l}")),
                }
            }
            ops.push(Op::Update {
                path: p.trim().into(),
                move_to,
                hunks,
            });
        } else {
            return Err(format!(
                "line {}: expected a `*** Add/Delete/Update File:` header, got: {l}",
                i + 1
            ));
        }
    }
    Ok(ops)
}

/// Find `old` in `lines` at or after `from`: exact, then ignoring trailing, then all surrounding whitespace.
fn seek(lines: &[String], old: &[String], from: usize) -> Option<usize> {
    let fits = |f: &dyn Fn(&str) -> String| {
        (from..=lines.len().checked_sub(old.len())?)
            .find(|&s| old.iter().enumerate().all(|(k, o)| f(&lines[s + k]) == f(o)))
    };
    fits(&|s| s.to_string())
        .or_else(|| fits(&|s| s.trim_end().to_string()))
        .or_else(|| fits(&|s| s.trim().to_string()))
}

fn apply_hunks(text: &str, hunks: &[Hunk]) -> Result<String, String> {
    let mut lines: Vec<String> = text.lines().map(String::from).collect();
    let mut cursor = 0;
    for (n, h) in hunks.iter().enumerate() {
        if !h.header.is_empty() {
            let hdr = [h.header.clone()];
            if let Some(at) = seek(&lines, &hdr, cursor) {
                cursor = at + 1;
            }
        }
        if h.old.is_empty() {
            lines.extend(h.new.iter().cloned()); // pure addition: append
            continue;
        }
        let at = seek(&lines, &h.old, cursor).ok_or_else(|| {
            format!(
                "hunk {} does not match the file; context starts with: {:?}",
                n + 1,
                h.old.first().unwrap_or(&String::new())
            )
        })?;
        lines.splice(at..at + h.old.len(), h.new.iter().cloned());
        cursor = at + h.new.len();
    }
    let mut out = lines.join("\n");
    if !out.is_empty() {
        out.push('\n');
    }
    Ok(out)
}

impl Tool for ApplyPatch {
    fn name(&self) -> &str {
        "apply_patch"
    }
    fn guidelines(&self) -> &[&str] {
        &[
            "Change files with `apply_patch`, not with sed, perl or scripts run through bash (strictly forbidden unless no tool can do it). One patch can change several files.",
        ]
    }
    fn description(&self) -> &str {
        "Edit files with a patch: *** Begin Patch, then *** Add File: <path> (lines with +), *** Delete File: <path>, or *** Update File: <path> (optional *** Move to: <path>) with @@ hunks of ' ' context, '-' removed and '+' added lines; then *** End Patch."
    }
    fn schema(&self) -> Value {
        schema_for::<Args>()
    }
    fn large_output(&self) -> bool {
        false
    }
    fn sequential(&self) -> bool {
        true // one patch may touch many files; keep it out of parallel batches
    }
    fn execute<'a>(&'a self, args: Value, ctx: &'a ToolCtx) -> BoxFuture<'a, ToolOutput> {
        Box::pin(async move {
            let a: Args = match parse(args) {
                Ok(a) => a,
                Err(e) => return e,
            };
            let ops = match parse_patch(&a.input) {
                Ok(o) => o,
                Err(e) => return ToolOutput::error(format!("bad patch: {e}")),
            };
            // Plan every write first; nothing touches disk until all hunks apply.
            let mut writes: Vec<(String, Option<String>)> = Vec::new(); // path, None = delete
            let mut guards = Vec::new();
            for op in &ops {
                let p = match op {
                    Op::Add { path, .. } | Op::Delete { path } | Op::Update { path, .. } => path,
                };
                let g = ctx.files.lock(ctx, &ctx.path(p)).await;
                if let Some(stale) = ctx.files.check_stale(ctx, &g.path).await {
                    return ToolOutput::error(stale);
                }
                guards.push(g);
                match op {
                    Op::Add { path, content } => writes.push((path.clone(), Some(content.clone()))),
                    Op::Delete { path } => writes.push((path.clone(), None)),
                    Op::Update { path, move_to, hunks } => {
                        let text = match ctx.env.read(&ctx.path(path)).await {
                            Ok(b) => String::from_utf8_lossy(&b).into_owned(),
                            Err(e) => {
                                return ToolOutput::error(format!("cannot read {path}: {e}. No changes applied."));
                            }
                        };
                        let new = match apply_hunks(&text, hunks) {
                            Ok(t) => t,
                            Err(e) => return ToolOutput::error(format!("{path}: {e}. No changes applied.")),
                        };
                        match move_to {
                            Some(dest) => {
                                writes.push((dest.clone(), Some(new)));
                                writes.push((path.clone(), None));
                            }
                            None => writes.push((path.clone(), Some(new))),
                        }
                    }
                }
            }
            let mut done = Vec::new();
            for (p, content) in &writes {
                let full = ctx.path(p);
                let r = match content {
                    Some(c) => ctx.env.write(&full, c.as_bytes()).await,
                    None => ctx.env.remove(&full).await,
                };
                if let Err(e) = r {
                    return ToolOutput::error(format!("{p}: {e}. Applied before the failure: {done:?}"));
                }
                if content.is_some() {
                    ctx.files.record(ctx, &ctx.env.canonical(&full).await).await;
                }
                done.push(format!("{} {p}", if content.is_some() { "M" } else { "D" }));
            }
            drop(guards);
            ToolOutput::text(format!("Applied patch:\n{}", done.join("\n")))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{ctx, text};
    use serde_json::json;

    #[tokio::test]
    async fn add_update_move_delete() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("a.rs"), "fn main() {\n    old();\n}\n").unwrap();
        std::fs::write(d.path().join("gone.txt"), "x\n").unwrap();
        let c = ctx(d.path());
        let patch = "*** Begin Patch\n*** Add File: new/b.txt\n+hello\n*** Update File: a.rs\n*** Move to: c.rs\n@@ fn main() {\n-    old();\n+    new();\n*** Delete File: gone.txt\n*** End Patch";
        let o = ApplyPatch.execute(json!({"input": patch}), &c).await;
        assert!(!o.is_error, "{}", text(&o));
        assert_eq!(std::fs::read_to_string(d.path().join("new/b.txt")).unwrap(), "hello\n");
        assert_eq!(
            std::fs::read_to_string(d.path().join("c.rs")).unwrap(),
            "fn main() {\n    new();\n}\n"
        );
        assert!(!d.path().join("a.rs").exists() && !d.path().join("gone.txt").exists());
    }

    #[tokio::test]
    async fn bad_hunk_changes_nothing() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("a"), "one\n").unwrap();
        let c = ctx(d.path());
        let patch = "*** Begin Patch\n*** Add File: b\n+x\n*** Update File: a\n-zzz\n+y\n*** End Patch";
        let o = ApplyPatch.execute(json!({"input": patch}), &c).await;
        assert!(o.is_error && text(&o).contains("No changes applied"));
        assert!(!d.path().join("b").exists());
        assert!(parse_patch("nope").is_err());
    }
}
