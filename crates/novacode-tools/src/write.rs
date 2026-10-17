//! write (L5.5): create parent dirs, atomic replace, file queue, stale guard.

use crate::{Tool, ToolCtx, ToolOutput, parse, schema_for};
use futures_util::future::BoxFuture;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

pub struct Write;

#[derive(Deserialize, JsonSchema)]
struct Args {
    /// File path, relative to the working directory or absolute.
    path: String,
    /// Full new content of the file.
    content: String,
}

impl Tool for Write {
    fn name(&self) -> &str {
        "write"
    }
    fn description(&self) -> &str {
        "Create or overwrite a file with the given content. Creates missing parent directories. Prefer edit for changes to existing files."
    }
    fn schema(&self) -> Value {
        schema_for::<Args>()
    }
    fn large_output(&self) -> bool {
        false
    }
    fn execute<'a>(&'a self, args: Value, ctx: &'a ToolCtx) -> BoxFuture<'a, ToolOutput> {
        Box::pin(async move {
            let a: Args = match parse(args) {
                Ok(a) => a,
                Err(e) => return e,
            };
            let path = ctx.path(&a.path);
            let g = ctx.files.lock(ctx, &path).await;
            if let Some(stale) = ctx.files.check_stale(ctx, &g.path).await {
                return ToolOutput::error(stale);
            }
            if let Err(e) = ctx.env.write(&path, a.content.as_bytes()).await {
                return ToolOutput::error(format!("cannot write {}: {e}", a.path));
            }
            let real = ctx.env.canonical(&path).await;
            ctx.files.record(ctx, &real).await;
            ToolOutput::text(format!("Wrote {} bytes to {}", a.content.len(), a.path))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::ctx;
    use serde_json::json;

    #[tokio::test]
    async fn creates_dirs_and_refuses_stale() {
        let d = tempfile::tempdir().unwrap();
        let c = ctx(d.path());
        let o = Write.execute(json!({"path": "x/y/f.txt", "content": "hi"}), &c).await;
        assert!(!o.is_error, "{o:?}");
        assert_eq!(std::fs::read_to_string(d.path().join("x/y/f.txt")).unwrap(), "hi");
        // Someone else changes the file: the next write is refused.
        let real = std::fs::canonicalize(d.path().join("x/y/f.txt")).unwrap();
        c.files.saw(&real, 1);
        assert!(
            Write
                .execute(json!({"path": "x/y/f.txt", "content": "again"}), &c)
                .await
                .is_error
        );
    }
}
