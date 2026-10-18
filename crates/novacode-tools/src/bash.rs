//! bash (L5.8): process-group shell with timeout, streamed output tail, bounded buffer.

use crate::env::ExecSpec;
use crate::{Keep, Tool, ToolCtx, ToolOutput, parse, schema_for};
use futures_util::future::BoxFuture;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;
use std::time::Duration;
use tokio::sync::mpsc;

pub struct Bash;

pub const DEFAULT_TIMEOUT: u64 = 120;
pub const MAX_TIMEOUT: u64 = 600;
/// In-memory output cap; beyond it the oldest bytes are dropped (the guard spills the rest).
const BUFFER_CAP: usize = 4 * 1024 * 1024;

#[derive(Deserialize, JsonSchema)]
struct Args {
    /// Shell command, run with bash -c in the working directory.
    command: String,
    /// Timeout in seconds (default 120, max 600).
    timeout: Option<u64>,
    /// One short sentence on why you run this; shown to the user.
    intent: Option<String>,
    /// Only when the safety check asks for it: why this destructive command is needed and safe.
    justification: Option<String>,
}

impl Tool for Bash {
    fn name(&self) -> &str {
        "bash"
    }
    fn guidelines(&self) -> &[&str] {
        &[
            "Commands run without a terminal and can't take input: use non-interactive forms (`-y`, `--no-pager`, `--no-edit`, `CI=1`), never commands that prompt or open an editor or pager.",
        ]
    }
    fn description(&self) -> &str {
        "Run a shell command. stdout and stderr are merged; long output keeps the tail. stdin is closed, so interactive commands fail instead of waiting."
    }
    fn schema(&self) -> Value {
        schema_for::<Args>()
    }
    fn keep(&self) -> Keep {
        Keep::Tail
    }
    fn execute<'a>(&'a self, args: Value, ctx: &'a ToolCtx) -> BoxFuture<'a, ToolOutput> {
        Box::pin(async move {
            let a: Args = match parse(args) {
                Ok(a) => a,
                Err(e) => return e,
            };
            let _ = (a.intent, a.justification); // read by the UI and the policy from the call arguments
            let secs = a.timeout.unwrap_or(DEFAULT_TIMEOUT).clamp(1, MAX_TIMEOUT);
            let spec = ExecSpec {
                command: a.command,
                cwd: ctx.cwd.clone(),
                env: vec![("NOVACODE_SESSION_ID".into(), ctx.session_id.clone())],
                timeout: Duration::from_secs(secs),
            };
            let (tx, mut rx) = mpsc::channel::<Vec<u8>>(64);
            let run = ctx.env.exec(spec, ctx.cancel.clone(), tx);
            let mut buf: Vec<u8> = Vec::new();
            let mut dropped = 0usize;
            let collect = async {
                while let Some(chunk) = rx.recv().await {
                    ctx.update(String::from_utf8_lossy(&chunk).into_owned());
                    buf.extend_from_slice(&chunk);
                    if buf.len() > BUFFER_CAP {
                        let cut = buf.len() - BUFFER_CAP;
                        buf.drain(..cut);
                        dropped += cut;
                    }
                }
            };
            let (res, ()) = tokio::join!(run, collect);
            let mut out = String::new();
            if dropped > 0 {
                out.push_str(&format!("[first {dropped} bytes of output dropped]\n"));
            }
            out.push_str(&String::from_utf8_lossy(&buf));
            let res = match res {
                Ok(r) => r,
                Err(e) => return ToolOutput::error(format!("could not start bash: {e}")),
            };
            if !out.is_empty() && !out.ends_with('\n') {
                out.push('\n');
            }
            // shortcut: on timeout the command is killed; L5.8 hands it to the process manager instead (M9).
            let (status, is_error) = if res.timed_out {
                (
                    format!(
                        "[killed after the {secs}s timeout; pass a larger timeout (max {MAX_TIMEOUT}s) if it needs longer]"
                    ),
                    true,
                )
            } else if res.cancelled {
                ("[cancelled]".to_string(), true)
            } else {
                match res.code {
                    Some(0) => (String::new(), false),
                    Some(c) => (format!("[exit code {c}]"), true),
                    None => ("[killed by a signal]".to_string(), true),
                }
            };
            if out.is_empty() && status.is_empty() {
                out.push_str("(no output)");
            }
            out.push_str(&status);
            ToolOutput {
                is_error,
                ..ToolOutput::text(out.trim_end().to_string())
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
    async fn runs_merges_and_reports() {
        let d = tempfile::tempdir().unwrap();
        let c = ctx(d.path());
        let o = Bash
            .execute(
                json!({"command": "echo out; echo err >&2; echo $NOVACODE_SESSION_ID; pwd"}),
                &c,
            )
            .await;
        let t = text(&o);
        assert!(
            !o.is_error && t.contains("out") && t.contains("err") && t.contains("test"),
            "{t}"
        );
        let o = Bash.execute(json!({"command": "exit 3"}), &c).await;
        assert!(o.is_error && text(&o).contains("exit code 3"));
    }

    #[tokio::test]
    async fn timeout_kills_the_group() {
        let d = tempfile::tempdir().unwrap();
        let c = ctx(d.path());
        let t0 = std::time::Instant::now();
        // The background child keeps stdout open; the group kill must still end the call fast.
        let o = Bash
            .execute(
                json!({"command": "sleep 30 & echo started; sleep 30", "timeout": 1}),
                &c,
            )
            .await;
        assert!(o.is_error && text(&o).contains("timeout"), "{}", text(&o));
        assert!(t0.elapsed() < Duration::from_secs(5));
    }

    #[tokio::test]
    async fn cancel_stops_it() {
        let d = tempfile::tempdir().unwrap();
        let c = ctx(d.path());
        let cancel = c.cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            cancel.cancel();
        });
        let o = Bash.execute(json!({"command": "sleep 30"}), &c).await;
        assert!(text(&o).contains("[cancelled]"));
    }
}
