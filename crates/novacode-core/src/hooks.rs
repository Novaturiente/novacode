//! Hooks (L7.1, L7.3): handlers the tool pipeline asks before and after each
//! call, plus observers of the session's events. Command hooks from settings are
//! one implementation; script extensions (L7.2) are another.
//!
//! Combine rules: `before_tool` chains (each sees the previous one's args; the
//! first block wins), `after_tool` collects feedback text in order, events are
//! observed only.

use futures_util::future::BoxFuture;
use novacode_tools::ToolOutput;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::AsyncWriteExt;

/// What a `before_tool` handler decided.
pub enum Gate {
    Allow,
    /// Run with these arguments instead.
    Args(Value),
    /// Don't run; the reason goes to the model as the tool's error.
    Block(String),
}

pub trait Handler: Send + Sync {
    /// Tag of its feedback in the result (`[hook] …`, `[diagnostics] …`).
    fn label(&self) -> &str {
        "hook"
    }
    fn before_tool<'a>(&'a self, _tool: &'a str, _args: &'a Value) -> BoxFuture<'a, Gate> {
        Box::pin(async { Gate::Allow })
    }
    /// Text to append to the result the model sees.
    fn after_tool<'a>(
        &'a self,
        _tool: &'a str,
        _args: &'a Value,
        _out: &'a ToolOutput,
    ) -> BoxFuture<'a, Option<String>> {
        Box::pin(async { None })
    }
}

#[derive(Default, Clone)]
pub struct Hooks {
    pub handlers: Vec<Arc<dyn Handler>>,
}

impl Hooks {
    pub fn is_empty(&self) -> bool {
        self.handlers.is_empty()
    }

    /// Chain the handlers: `Ok(args)` to run with, or `Err(output)` to return instead.
    pub async fn before_tool(&self, tool: &str, mut args: Value) -> Result<Value, ToolOutput> {
        for h in &self.handlers {
            match h.before_tool(tool, &args).await {
                Gate::Allow => {}
                Gate::Args(a) if a.is_object() => args = a,
                Gate::Args(_) => {}
                Gate::Block(why) => return Err(ToolOutput::error(format!("Blocked by a hook: {why}"))),
            }
        }
        Ok(args)
    }

    /// Append every handler's feedback to `out` (capped per handler).
    pub async fn after_tool(&self, tool: &str, args: &Value, out: &mut ToolOutput) {
        for h in &self.handlers {
            if let Some(text) = h.after_tool(tool, args, out).await.filter(|t| !t.trim().is_empty()) {
                let text: String = text.trim().chars().take(8000).collect();
                out.content
                    .push(novacode_types::Block::text(format!("[{}] {text}", h.label())));
            }
        }
    }
}

/// `hooks` in settings: shell commands (one string or a list).
#[derive(Debug, Default, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Config {
    /// Before each tool call. Gets `{tool, args, …}` on stdin. Exit 2 blocks
    /// (stderr is the reason); stdout `{"args": {…}}` replaces the arguments.
    #[serde(default, deserialize_with = "one_or_many")]
    pub before_tool: Vec<String>,
    /// After each tool call; its stdout is appended to the result the model sees.
    #[serde(default, deserialize_with = "one_or_many")]
    pub after_tool: Vec<String>,
    /// Observers by event type (`agent_end`, `turn_start`, `tool_end`,
    /// `compacted`, `session_start`, `session_end`, …): the event JSON on stdin.
    #[serde(default)]
    pub events: std::collections::BTreeMap<String, OneOrMany>,
    /// Gate timeout (default 5000); after-tool hooks get twice as long.
    #[serde(default)]
    pub timeout_ms: Option<u64>,
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(untagged)]
pub enum OneOrMany {
    One(String),
    Many(Vec<String>),
}

impl OneOrMany {
    fn list(&self) -> Vec<String> {
        match self {
            OneOrMany::One(s) => vec![s.clone()],
            OneOrMany::Many(v) => v.clone(),
        }
    }
}

fn one_or_many<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Vec<String>, D::Error> {
    Ok(<Option<OneOrMany> as serde::Deserialize>::deserialize(d)?
        .map(|o| o.list())
        .unwrap_or_default())
}

/// Set for every hook command, so a novacode started inside one runs no hooks.
pub const DISABLED_ENV: &str = "NOVACODE_HOOKS_DISABLED";

pub fn disabled() -> bool {
    std::env::var_os(DISABLED_ENV).is_some()
}

/// Shell command hooks for one session.
pub struct Commands {
    pub config: Config,
    pub cwd: PathBuf,
    pub session: String,
}

/// Exit status, stdout and stderr; `None` on spawn failure or timeout.
async fn run(
    cmd: &str,
    event: &str,
    cwd: &Path,
    session: &str,
    input: &Value,
    timeout: Duration,
) -> Option<(Option<i32>, String, String)> {
    let mut c = tokio::process::Command::new("sh");
    c.arg("-c")
        .arg(cmd)
        .current_dir(cwd)
        .env(DISABLED_ENV, "1")
        .env("NOVACODE_HOOK_EVENT", event)
        .env("NOVACODE_SESSION_ID", session)
        .env("NOVACODE_CWD", cwd)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .process_group(0)
        .kill_on_drop(true);
    if let Some(t) = input["tool"].as_str() {
        c.env("NOVACODE_TOOL", t);
    }
    let mut child = c
        .spawn()
        .map_err(|e| tracing::warn!(cmd, "hook did not start: {e}"))
        .ok()?;
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(input.to_string().as_bytes()).await;
    }
    match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Ok(Ok(o)) => Some((
            o.status.code(),
            String::from_utf8_lossy(&o.stdout).into_owned(),
            String::from_utf8_lossy(&o.stderr).into_owned(),
        )),
        Ok(Err(e)) => {
            tracing::warn!(cmd, "hook failed: {e}");
            None
        }
        Err(_) => {
            tracing::warn!(cmd, "hook timed out after {}ms (fail-open)", timeout.as_millis());
            None
        }
    }
}

impl Commands {
    fn timeout(&self) -> Duration {
        Duration::from_millis(self.config.timeout_ms.unwrap_or(5000))
    }
}

impl Handler for Commands {
    fn before_tool<'a>(&'a self, tool: &'a str, args: &'a Value) -> BoxFuture<'a, Gate> {
        Box::pin(async move {
            let mut args = args.clone();
            let mut changed = false;
            for cmd in &self.config.before_tool {
                let input = json!({ "event": "before_tool", "tool": tool, "args": args, "session": self.session, "cwd": self.cwd });
                // Fail open: anything but exit 0/2 (or a timeout) lets the call through.
                let Some((code, out, err)) =
                    run(cmd, "before_tool", &self.cwd, &self.session, &input, self.timeout()).await
                else {
                    continue;
                };
                match code {
                    Some(2) => {
                        let why: String = err.trim().chars().take(2000).collect();
                        return Gate::Block(if why.is_empty() {
                            format!("`{cmd}` refused it")
                        } else {
                            why
                        });
                    }
                    Some(0) => {
                        if let Ok(v) = serde_json::from_str::<Value>(out.trim())
                            && v["args"].is_object()
                        {
                            args = v["args"].clone();
                            changed = true;
                        }
                    }
                    _ => tracing::warn!(cmd, ?code, "before_tool hook failed (fail-open)"),
                }
            }
            if changed { Gate::Args(args) } else { Gate::Allow }
        })
    }

    fn after_tool<'a>(&'a self, tool: &'a str, args: &'a Value, out: &'a ToolOutput) -> BoxFuture<'a, Option<String>> {
        Box::pin(async move {
            if self.config.after_tool.is_empty() {
                return None;
            }
            let text: String = out
                .content
                .iter()
                .filter_map(|b| match b {
                    novacode_types::Block::Text { text, .. } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n");
            let preview: String = text.chars().take(16_000).collect();
            let input = json!({ "event": "after_tool", "tool": tool, "args": args, "is_error": out.is_error, "output": preview, "session": self.session, "cwd": self.cwd });
            let mut feedback = Vec::new();
            for cmd in &self.config.after_tool {
                if let Some((_, out, _)) =
                    run(cmd, "after_tool", &self.cwd, &self.session, &input, self.timeout() * 2).await
                    && !out.trim().is_empty()
                {
                    feedback.push(out.trim().to_string());
                }
            }
            (!feedback.is_empty()).then(|| feedback.join("\n"))
        })
    }
}

/// Observer runs still going; [`flush`] waits for them before the process exits.
static RUNNING: std::sync::Mutex<Vec<tokio::task::JoinHandle<()>>> = std::sync::Mutex::new(Vec::new());

/// Wait (up to `max`) for observer commands still running, so `-p` runs don't drop them.
pub async fn flush(max: Duration) {
    let all: Vec<_> = std::mem::take(&mut *RUNNING.lock().unwrap());
    let _ = tokio::time::timeout(max, futures_util::future::join_all(all)).await;
}

/// Run event observers for each matching session event, detached (fire-and-forget).
pub fn spawn_observers(
    commands: Arc<Commands>,
    mut events: tokio::sync::broadcast::Receiver<novacode_types::AgentEvent>,
) -> Option<tokio::task::JoinHandle<()>> {
    if commands.config.events.is_empty() {
        return None;
    }
    let fire = |c: &Arc<Commands>, name: &str, payload: Value| {
        for cmd in c.config.events.get(name).map(|o| o.list()).unwrap_or_default() {
            let (c, name, payload) = (c.clone(), name.to_string(), payload.clone());
            let h = tokio::spawn(async move {
                run(&cmd, &name, &c.cwd, &c.session, &payload, Duration::from_secs(30)).await;
            });
            let mut r = RUNNING.lock().unwrap();
            r.retain(|h| !h.is_finished());
            r.push(h);
        }
    };
    fire(
        &commands,
        "session_start",
        json!({ "event": "session_start", "session": commands.session, "cwd": commands.cwd }),
    );
    Some(tokio::spawn(async move {
        loop {
            match events.recv().await {
                Ok(ev) => {
                    let v = serde_json::to_value(&ev).unwrap_or_default();
                    let name = v["type"].as_str().unwrap_or_default().to_string();
                    // Streaming deltas and live tool output are too frequent for processes.
                    if name != "stream" && name != "tool_update" {
                        fire(&commands, &name, v);
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(_) => break,
            }
        }
        let payload = json!({ "event": "session_end", "session": commands.session, "cwd": commands.cwd });
        for cmd in commands
            .config
            .events
            .get("session_end")
            .map(|o| o.list())
            .unwrap_or_default()
        {
            // Waited for: the process is about to exit.
            run(
                &cmd,
                "session_end",
                &commands.cwd,
                &commands.session,
                &payload,
                Duration::from_secs(5),
            )
            .await;
        }
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(o: &ToolOutput) -> String {
        o.content
            .iter()
            .filter_map(|b| match b {
                novacode_types::Block::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn commands(config: Value, dir: &Path) -> Commands {
        Commands {
            config: serde_json::from_value(config).unwrap(),
            cwd: dir.into(),
            session: "s1".into(),
        }
    }

    #[tokio::test]
    async fn gate_transform_and_feedback() {
        let d = tempfile::tempdir().unwrap();
        let c = commands(
            json!({
                "beforeTool": [
                    "grep -q 'rm -rf' && { echo 'no deleting' >&2; exit 2; }; exit 0",
                    "[ \"$NOVACODE_TOOL\" = read ] && echo '{\"args\":{\"path\":\"b.txt\"}}' || true",
                    "exit 7"
                ],
                "afterTool": "echo \"lint: $NOVACODE_TOOL ok\""
            }),
            d.path(),
        );
        let hooks = Hooks {
            handlers: vec![Arc::new(c)],
        };
        let blocked = hooks
            .before_tool("bash", json!({"command": "rm -rf x"}))
            .await
            .unwrap_err();
        assert!(blocked.is_error && text(&blocked).contains("no deleting"));
        let args = hooks.before_tool("read", json!({"path": "a.txt"})).await.unwrap();
        assert_eq!(args, json!({"path": "b.txt"}), "transformed; exit 7 fails open");
        let mut out = ToolOutput::text("contents");
        hooks.after_tool("read", &args, &mut out).await;
        assert_eq!(text(&out), "contents\n[hook] lint: read ok");
    }
}
