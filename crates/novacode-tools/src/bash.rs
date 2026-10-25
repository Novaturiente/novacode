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
    /// Run outside the sandbox, only for a command that needs what it hides or blocks (e.g. `git push` over SSH, writing outside the project).
    #[serde(default)]
    unsandboxed: bool,
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
        "Run a shell command. stdout and stderr are merged; long output keeps the tail. stdin is closed, so interactive commands fail instead of waiting. Each call is a fresh shell: cd and exports don't carry over. On timeout the command keeps running in the background with an id."
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
                unsandboxed: a.unsandboxed,
            };
            if let Some(pm) = &ctx.processes {
                return run_managed(pm, spec, secs, ctx).await;
            }
            let git = git_snapshot(ctx, a.unsandboxed);
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
            let note = sandbox_note(ctx, a.unsandboxed, is_error, &out);
            if out.is_empty() && status.is_empty() {
                out.push_str("(no output)");
            }
            out.push_str(&status);
            push_note(&mut out, note);
            push_note(&mut out, git.and_then(|g| g.changes()));
            ToolOutput {
                is_error,
                ..ToolOutput::text(out.trim_end().to_string())
            }
        })
    }
}

/// L6.5: the git setup before a sandboxed command, for the change check after it.
fn git_snapshot(ctx: &ToolCtx, unsandboxed: bool) -> Option<crate::sandbox::GitSnap> {
    ctx.env.sandbox().filter(|_| !unsandboxed)?.git_snapshot()
}

/// L6.5: the mark of a command run outside the sandbox, or the hint for a
/// sandboxed one that failed on a blocked write.
pub(crate) fn sandbox_note(ctx: &ToolCtx, unsandboxed: bool, failed: bool, out: &str) -> Option<String> {
    let sb = ctx.env.sandbox();
    if unsandboxed {
        return sb
            .filter(|s| s.wraps())
            .map(|_| "[ran outside the sandbox]".to_string());
    }
    crate::sandbox::hint_for(sb, failed, out)
}

fn push_note(out: &mut String, note: Option<String>) {
    if let Some(n) = note {
        if !out.ends_with('\n') {
            out.push('\n');
        }
        out.push_str(&n);
    }
}

/// Through the process manager (L5.8): on timeout or Ctrl+Alt+B the command keeps
/// running there and the model gets its id; at twice the timeout it is asked
/// whether to keep it.
// shortcut: managed commands run locally, not through ExecutionEnv; route spawn through it with the SSH backend (L8.11).
async fn run_managed(
    pm: &std::sync::Arc<crate::process::ProcessManager>,
    spec: ExecSpec,
    secs: u64,
    ctx: &ToolCtx,
) -> ToolOutput {
    let name: String = spec.command.lines().next().unwrap_or("").chars().take(40).collect();
    let sb = if spec.unsandboxed { None } else { ctx.env.sandbox() };
    let id = match pm.start(&name, &spec.command, &spec.cwd, &spec.env, false, sb) {
        Ok(id) => id,
        Err(e) => return ToolOutput::error(format!("could not start bash: {e}")),
    };
    // bash calls get no input: a prompt sees EOF at once instead of hanging (the `process` tool keeps stdin).
    let _ = pm.write(&id, "", true).await;
    let mut buf: Vec<u8> = Vec::new();
    let mut dropped = 0usize;
    let ended = tokio::select! {
        e = pm.wait(&id, spec.timeout, |c| {
            ctx.update(String::from_utf8_lossy(c).into_owned());
            buf.extend_from_slice(c);
            if buf.len() > BUFFER_CAP {
                let cut = buf.len() - BUFFER_CAP;
                buf.drain(..cut);
                dropped += cut;
            }
        }) => e,
        _ = ctx.cancel.cancelled() => {
            pm.stop(&id).await;
            pm.clear_one(&id);
            return ToolOutput { is_error: true, ..ToolOutput::text("[cancelled]".to_string()) };
        }
    };
    let mut out = String::new();
    if dropped > 0 {
        out.push_str(&format!("[first {dropped} bytes of output dropped]\n"));
    }
    out.push_str(&String::from_utf8_lossy(&buf));
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    let Some(end) = ended else {
        pm.reveal(&id, crate::process::Attention::Wake);
        pm.ask_at(&id, novacode_base_now_ms() + secs * 1000);
        out.push_str(&format!(
            "[still running: moved to the process manager as {id}. Use the `process` tool to read its output or stop it; you'll be told when it exits]"
        ));
        let note = sandbox_note(ctx, spec.unsandboxed, false, &out);
        push_note(&mut out, note);
        return ToolOutput::text(out.trim_end().to_string());
    };
    // The process manager ran the git change check when it exited.
    let git = pm.git_note(&id);
    pm.clear_one(&id);
    let (status, is_error) = match end.code {
        Some(0) => (String::new(), false),
        Some(c) => (format!("[exit code {c}]"), true),
        None => ("[killed by a signal]".to_string(), true),
    };
    let note = sandbox_note(ctx, spec.unsandboxed, is_error, &out);
    if out.is_empty() && status.is_empty() {
        out.push_str("(no output)");
    }
    out.push_str(&status);
    push_note(&mut out, note);
    push_note(&mut out, git);
    ToolOutput {
        is_error,
        ..ToolOutput::text(out.trim_end().to_string())
    }
}

fn novacode_base_now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
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

    /// L5.8: a timed-out command keeps running in the process manager.
    #[tokio::test]
    async fn timeout_hands_over_to_the_process_manager() {
        let d = tempfile::tempdir().unwrap();
        let mut c = ctx(d.path());
        let pm = crate::process::ProcessManager::new(&d.path().join("procs"), "s");
        c.processes = Some(pm.clone());
        let o = Bash
            .execute(json!({"command": "echo up; sleep 2; echo later", "timeout": 1}), &c)
            .await;
        let t = text(&o);
        assert!(
            !o.is_error && t.contains("up") && t.contains("moved to the process manager as proc_1"),
            "{t}"
        );
        assert_eq!(pm.running(), 1);
        tokio::time::sleep(Duration::from_millis(1800)).await;
        assert!(pm.output("proc_1", 10, None).unwrap().contains("later"));
        assert_eq!(pm.running(), 0);
        // A quick one leaves nothing behind; stdin is closed, so `read` doesn't wait.
        let o = Bash.execute(json!({"command": "read x; echo fine"}), &c).await;
        assert_eq!(text(&o), "fine");
        assert_eq!(pm.list().len(), 1);
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
