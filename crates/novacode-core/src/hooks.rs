//! Hooks (L7.1, L7.3): handlers the tool pipeline asks before and after each
//! call, plus observers of the session's events. Command hooks from settings are
//! one implementation; script extensions (L7.2) are another.
//!
//! Combine rules: `before_tool` chains (each sees the previous one's args; the
//! first block wins, then ask over allow), `after_tool` runs in order: each may append feedback,
//! replace the result text or flip its error flag. Events are observed only.

use crate::policy::HookDecision;
use futures_util::future::BoxFuture;
use novacode_tools::ToolOutput;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};
use tokio::io::AsyncWriteExt;

/// What a finished tool call did, for `after_tool` handlers.
#[derive(Debug, Clone, Default)]
pub struct Run {
    pub call_id: String,
    pub duration_ms: u64,
    /// Absolute paths the call changed (or, for `read`, looked at).
    pub touched: Vec<PathBuf>,
}

impl Run {
    /// The keys every after-tool payload carries.
    pub fn json(&self) -> Value {
        json!({ "callId": self.call_id, "durationMs": self.duration_ms, "touched": self.touched })
    }
}

/// An `after_tool` handler's answer: text to append, a replacement result, a new error flag.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct After {
    pub append: Option<String>,
    pub replace: Option<String>,
    pub is_error: Option<bool>,
}

impl After {
    pub fn append(text: impl Into<String>) -> Self {
        After {
            append: Some(text.into()),
            ..Default::default()
        }
    }

    /// A handler's JSON answer: a string appends; `{append, replace, isError}` does each.
    pub fn from_json(v: &Value) -> Option<Self> {
        match v {
            Value::String(s) => Some(After::append(s.clone())),
            Value::Object(o) => Some(After {
                append: o.get("append").and_then(Value::as_str).map(String::from),
                replace: o.get("replace").and_then(Value::as_str).map(String::from),
                is_error: o.get("isError").and_then(Value::as_bool),
            }),
            _ => None,
        }
    }
}

/// Taken before a call runs; finished by [`Hooks::after_tool`].
pub struct Started {
    t0: Instant,
    snap: Option<Snap>,
}

/// Changed files under a folder (modified, untracked, deleted) → (mtime, len).
type Snap = HashMap<PathBuf, Option<(SystemTime, u64)>>;

/// shortcut: git-only (`ls-files -m -o -d`); a bash call outside a repo reports no touched files. Upgrade to an mtime walk if non-git projects need it.
async fn snapshot(cwd: &Path) -> Option<Snap> {
    let o = tokio::process::Command::from(novacode_base::git::command_in(cwd))
        .args(["ls-files", "-z", "-m", "-o", "-d", "--exclude-standard"])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .await
        .ok()
        .filter(|o| o.status.success())?;
    Some(
        o.stdout
            .split(|b| *b == 0)
            .filter(|p| !p.is_empty())
            .map(|p| {
                let path = cwd.join(String::from_utf8_lossy(p).as_ref());
                let meta = std::fs::metadata(&path)
                    .ok()
                    .and_then(|m| Some((m.modified().ok()?, m.len())));
                (path, meta)
            })
            .collect(),
    )
}

/// Files whose state differs between two snapshots.
fn diff(before: &Snap, after: &Snap) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = after
        .iter()
        .filter(|(p, m)| before.get(*p) != Some(m))
        .map(|(p, _)| p.clone())
        .chain(before.keys().filter(|p| !after.contains_key(*p)).cloned())
        .collect();
    v.sort();
    v.dedup();
    v
}

/// Paths a tool call names: `path`, or the files in an apply_patch.
pub fn named(tool: &str, args: &Value, cwd: &Path) -> Vec<PathBuf> {
    let abs = |p: &str| {
        let p = Path::new(p);
        if p.is_absolute() { p.to_path_buf() } else { cwd.join(p) }
    };
    match tool {
        "edit" | "write" | "read" => args["path"].as_str().map(abs).into_iter().collect(),
        "apply_patch" => {
            let text = args["patch"].as_str().or(args["input"].as_str()).unwrap_or_default();
            text.lines()
                .filter_map(|l| {
                    l.strip_prefix("*** Update File: ")
                        .or_else(|| l.strip_prefix("*** Add File: "))
                        .or_else(|| l.strip_prefix("*** Move to: "))
                })
                .map(|p| abs(p.trim()))
                .collect()
        }
        _ => Vec::new(),
    }
}

/// What a `before_tool` handler decided.
pub enum Gate {
    Allow,
    /// Run with these arguments instead.
    Args(Value),
    /// Don't run; the reason goes to the model as the tool's error.
    Block(String),
    /// A permission answer (plugin `PreToolUse`, L7.16), with rewritten arguments if any.
    Decide(HookDecision, Option<Value>),
}

pub trait Handler: Send + Sync {
    /// Tag of its feedback in the result (`[hook] …`, `[diagnostics] …`).
    fn label(&self) -> &str {
        "hook"
    }
    fn before_tool<'a>(&'a self, _tool: &'a str, _args: &'a Value) -> BoxFuture<'a, Gate> {
        Box::pin(async { Gate::Allow })
    }
    /// Feedback, a replacement or a new error flag for the result the model sees.
    fn after_tool<'a>(
        &'a self,
        _tool: &'a str,
        _args: &'a Value,
        _out: &'a ToolOutput,
        _run: &'a Run,
    ) -> BoxFuture<'a, Option<After>> {
        Box::pin(async { None })
    }
    /// Whether it reads [`Run::touched`] (costs a git snapshot around each `bash`).
    fn wants_touched(&self) -> bool {
        false
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

    /// Chain the handlers: `Ok((args, decision))` to run with, or `Err(output)` to return instead.
    /// A block wins over any decision; the decision goes to the permission check.
    pub async fn before_tool(&self, tool: &str, mut args: Value) -> Result<(Value, HookDecision), ToolOutput> {
        let mut decision = HookDecision::None;
        for h in &self.handlers {
            let new = match h.before_tool(tool, &args).await {
                Gate::Allow => None,
                Gate::Args(a) => Some(a),
                Gate::Decide(d, a) => {
                    decision = decision.merge(d);
                    a
                }
                Gate::Block(why) => return Err(ToolOutput::error(format!("Blocked by a hook: {why}"))),
            };
            if let Some(a) = new.filter(Value::is_object) {
                args = a;
            }
        }
        Ok((args, decision))
    }

    /// Before a call runs: start its clock, and snapshot the tree around `bash`
    /// when a handler reads touched files.
    pub async fn start(&self, tool: &str, cwd: &Path) -> Started {
        let snap = if tool == "bash" && self.handlers.iter().any(|h| h.wants_touched()) {
            snapshot(cwd).await
        } else {
            None
        };
        Started {
            t0: Instant::now(),
            snap,
        }
    }

    /// Run every handler in order on `out`: replace, flip the error flag, append feedback (capped).
    pub async fn after_tool(
        &self,
        call_id: &str,
        tool: &str,
        args: &Value,
        cwd: &Path,
        started: Started,
        out: &mut ToolOutput,
    ) {
        if self.handlers.is_empty() {
            return;
        }
        let mut touched = named(tool, args, cwd);
        if let (Some(before), Some(after)) = (&started.snap, snapshot_if(&started, cwd).await) {
            touched.extend(diff(before, &after));
        }
        let run = Run {
            call_id: call_id.into(),
            duration_ms: started.t0.elapsed().as_millis() as u64,
            touched,
        };
        for h in &self.handlers {
            let Some(a) = h.after_tool(tool, args, out, &run).await else {
                continue;
            };
            if let Some(r) = a.replace {
                out.content = vec![novacode_types::Block::text(r)];
            }
            if let Some(e) = a.is_error {
                out.is_error = e;
            }
            if let Some(text) = a.append.filter(|t| !t.trim().is_empty()) {
                let text: String = text.trim().chars().take(8000).collect();
                out.content
                    .push(novacode_types::Block::text(format!("[{}] {text}", h.label())));
            }
        }
    }
}

async fn snapshot_if(started: &Started, cwd: &Path) -> Option<Snap> {
    if started.snap.is_some() {
        snapshot(cwd).await
    } else {
        None
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
    let mut c = tokio::process::Command::new(novacode_base::platform::sh());
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
        .kill_on_drop(true);
    novacode_base::platform::own_group(c.as_std_mut());
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

    fn wants_touched(&self) -> bool {
        !self.config.after_tool.is_empty()
    }

    fn after_tool<'a>(
        &'a self,
        tool: &'a str,
        args: &'a Value,
        out: &'a ToolOutput,
        call: &'a Run,
    ) -> BoxFuture<'a, Option<After>> {
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
            let input = json!({ "event": "after_tool", "tool": tool, "args": args, "is_error": out.is_error, "output": preview, "session": self.session, "cwd": self.cwd, "call_id": call.call_id, "duration_ms": call.duration_ms, "touched": call.touched });
            let mut feedback = Vec::new();
            for cmd in &self.config.after_tool {
                if let Some((_, out, _)) =
                    run(cmd, "after_tool", &self.cwd, &self.session, &input, self.timeout() * 2).await
                    && !out.trim().is_empty()
                {
                    feedback.push(out.trim().to_string());
                }
            }
            (!feedback.is_empty()).then(|| After::append(feedback.join("\n")))
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

/// Built in: a `bash` command that writes files gets a reminder to use the file
/// tools, which keep the stale guard, claims, rewind and diffs. A nudge, never a
/// block. shortcut: text match on the command, so `printf > f` slips through and a
/// heredoc into /tmp is nudged; use `touched` if that proves noisy.
pub struct BashWrites;

pub fn writes_files(cmd: &str) -> bool {
    let heredoc_to_file = cmd.contains("<<") && cmd.replace("2>", "").replace(">&", "").contains('>');
    let inplace = ["sed -i", "perl -i", "perl -pi", "perl -0pi"]
        .iter()
        .any(|p| cmd.contains(p));
    let tee = cmd.split(['|', ';', '&']).any(|c| c.trim_start().starts_with("tee "));
    let script = ["write_text(", "writeFileSync(", "fs.writeFile("]
        .iter()
        .any(|p| cmd.contains(p))
        || (cmd.contains("open(") && ["'w'", "\"w\"", "'a'", "\"a\"", "'wb'"].iter().any(|m| cmd.contains(m)));
    heredoc_to_file || inplace || tee || script
}

impl Handler for BashWrites {
    fn label(&self) -> &str {
        "novacode"
    }
    fn after_tool<'a>(
        &'a self,
        tool: &'a str,
        args: &'a Value,
        _: &'a ToolOutput,
        _: &'a Run,
    ) -> BoxFuture<'a, Option<After>> {
        let hit = tool == "bash" && args["command"].as_str().is_some_and(writes_files);
        Box::pin(async move {
            hit.then(|| {
                After::append(
                    "This command wrote files through bash, which is forbidden unless no tool can do it. Use `write` for new files and `edit` for changes; for several files, make several calls in one message (they run in parallel) or one codemode script.",
                )
            })
        })
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn spots_bash_file_writes() {
        for c in [
            "cat > src/main.rs <<'EOF'\nfn main(){}\nEOF",
            "cat <<EOF > a.txt\nx\nEOF",
            "sed -i 's/a/b/' x.rs",
            "echo hi | tee out.txt",
            "python3 - <<'PY'\nopen('a','w').write('x')\nPY",
            "python3 -c \"import pathlib; pathlib.Path('a').write_text('x')\"",
        ] {
            assert!(super::writes_files(c), "{c}");
        }
        for c in [
            "cargo build 2>&1 | tail",
            "ls > /dev/null 2>&1",
            "grep -n foo src",
            "cat <<EOF | python3\nprint(1)\nEOF",
            "git commit -m x",
        ] {
            assert!(!super::writes_files(c), "{c}");
        }
    }
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
        let (args, _) = hooks.before_tool("read", json!({"path": "a.txt"})).await.unwrap();
        assert_eq!(args, json!({"path": "b.txt"}), "transformed; exit 7 fails open");
        let mut out = ToolOutput::text("contents");
        let started = hooks.start("read", d.path()).await;
        hooks.after_tool("c1", "read", &args, d.path(), started, &mut out).await;
        assert_eq!(text(&out), "contents\n[hook] lint: read ok");
    }

    struct Seen(std::sync::Mutex<Vec<PathBuf>>);
    impl Handler for Seen {
        fn wants_touched(&self) -> bool {
            true
        }
        fn after_tool<'a>(
            &'a self,
            _: &'a str,
            _: &'a Value,
            _: &'a ToolOutput,
            run: &'a Run,
        ) -> BoxFuture<'a, Option<After>> {
            *self.0.lock().unwrap() = run.touched.clone();
            Box::pin(async {
                Some(After {
                    replace: Some("new".into()),
                    is_error: Some(true),
                    append: Some("note".into()),
                })
            })
        }
    }

    #[tokio::test]
    async fn bash_touched_files_and_after_verdicts() {
        let d = tempfile::tempdir().unwrap();
        let g = |a: &[&str]| {
            assert!(
                std::process::Command::new("git")
                    .args(a)
                    .current_dir(d.path())
                    .output()
                    .unwrap()
                    .status
                    .success()
            )
        };
        g(&["init", "-q"]);
        std::fs::write(d.path().join("a.txt"), "1").unwrap();
        std::fs::write(d.path().join("b.txt"), "1").unwrap();
        g(&["add", "."]);
        g(&["-c", "user.email=t@t", "-c", "user.name=t", "commit", "-qm", "x"]);
        std::fs::write(d.path().join("b.txt"), "dirty before").unwrap();
        let seen = Arc::new(Seen(Default::default()));
        let hooks = Hooks {
            handlers: vec![seen.clone()],
        };
        let started = hooks.start("bash", d.path()).await;
        // What a `sed -i`/`cargo fmt` would do: change a clean file, add one; b.txt stays as it was.
        std::fs::write(d.path().join("a.txt"), "2").unwrap();
        std::fs::write(d.path().join("new.rs"), "x").unwrap();
        let mut out = ToolOutput::text("ran");
        hooks
            .after_tool("c1", "bash", &json!({"command": "x"}), d.path(), started, &mut out)
            .await;
        assert_eq!(
            *seen.0.lock().unwrap(),
            vec![d.path().join("a.txt"), d.path().join("new.rs")]
        );
        assert!(out.is_error);
        assert_eq!(text(&out), "new\n[hook] note");
        assert_eq!(
            named("edit", &json!({"path": "x.rs"}), d.path()),
            vec![d.path().join("x.rs")]
        );
    }
}
