//! Claude Code plugin hooks (L7.14): the command hooks of installed plugins, run
//! with Claude Code's stdin and stdout shape so a plugin works as it does there.
//!
//! | Event | In novacode |
//! |---|---|
//! | `SessionStart` | before the first model call of a session (`startup`/`resume`) and after a compaction (`compact`); its text is saved for the model |
//! | `SubagentStart` | instead of `SessionStart` in a subagent; matcher = agent type |
//! | `UserPromptSubmit` | before each prompt; text is saved after the prompt, a block drops the prompt |
//! | `PreToolUse` / `PostToolUse` | block, rewrite the input, add feedback (a [`Handler`]) |
//! | `Stop` | a block keeps the agent going with the reason |
//! | `SessionEnd` | when the session closes, observe only |
//!
//! Every hook fails open: a crash, a timeout or output it can't read changes nothing.

use crate::hooks::{After, DISABLED_ENV, Gate, Handler, Run};
use crate::packages::Package;
use crate::plugin::{Hook, Kind, Parts};
use futures_util::future::BoxFuture;
use novacode_tools::ToolOutput;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::io::AsyncWriteExt;

/// Claude Code tool names and novacode's.
pub const TOOLS: &[(&str, &str)] = &[
    ("Read", "read"),
    ("Write", "write"),
    ("Edit", "edit"),
    ("MultiEdit", "edit"),
    ("Bash", "bash"),
    ("Grep", "grep"),
    ("Glob", "find"),
    ("LS", "ls"),
    ("WebFetch", "web_fetch"),
    ("WebSearch", "web_search"),
    ("TodoWrite", "todo"),
    ("Task", "agent"),
];

/// A novacode tool name as Claude Code's (`server__tool` becomes `mcp__server__tool`).
pub fn claude_tool(t: &str) -> String {
    match TOOLS.iter().find(|(_, n)| *n == t) {
        Some((c, _)) => c.to_string(),
        None if t.contains("__") && !t.starts_with("mcp__") => format!("mcp__{t}"),
        None => t.to_string(),
    }
}

/// novacode arguments in Claude Code's shape: `path` is also `file_path`.
fn claude_input(args: &Value) -> Value {
    let mut v = args.clone();
    if let Some(o) = v.as_object_mut()
        && let Some(p) = o.get("path").cloned()
    {
        o.entry("file_path").or_insert(p);
    }
    v
}

struct Bound {
    hook: Hook,
    env: Vec<(String, String)>,
    parts: Parts,
}

/// What one hook printed: exit code, stdout, stderr.
struct Out {
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

impl Out {
    fn json(&self) -> Option<Value> {
        let t = self.stdout.trim();
        t.starts_with('{').then(|| serde_json::from_str(t).ok()).flatten()
    }
    /// Text for the model: `additionalContext`, else plain stdout (not JSON).
    fn context(&self) -> Option<String> {
        let t = match self.json() {
            Some(j) => j["hookSpecificOutput"]["additionalContext"].as_str()?.to_string(),
            None => self.stdout.trim().to_string(),
        };
        (!t.trim().is_empty()).then_some(t)
    }
    /// A block: exit 2 (stderr), `decision: block` (reason) or `continue: false`.
    fn block(&self) -> Option<String> {
        if self.code == Some(2) {
            return Some(self.stderr.trim().to_string());
        }
        let j = self.json()?;
        if j["decision"] == "block" {
            return Some(j["reason"].as_str().unwrap_or("blocked by a hook").to_string());
        }
        (j["continue"] == false).then(|| j["stopReason"].as_str().unwrap_or("stopped by a hook").to_string())
    }
}

/// The plugin hooks of one session.
pub struct PluginHooks {
    hooks: Vec<Bound>,
    cwd: PathBuf,
    session: String,
    /// Set in a subagent: its type (`SubagentStart` instead of `SessionStart`).
    pub subagent: Option<String>,
}

impl PluginHooks {
    pub fn load(packages: &[Package], cwd: &Path, session: &str, subagent: Option<String>) -> Self {
        let mut hooks = Vec::new();
        for p in packages {
            let parts = Parts::of(p);
            if parts.kind != Kind::Claude {
                continue;
            }
            let env = parts.env(&parts.options());
            for hook in parts.hooks().0 {
                hooks.push(Bound {
                    hook,
                    env: env.clone(),
                    parts: parts.clone(),
                });
            }
        }
        PluginHooks {
            hooks,
            cwd: cwd.to_path_buf(),
            session: session.to_string(),
            subagent,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.hooks.is_empty()
    }

    pub fn has(&self, event: &str) -> bool {
        self.hooks.iter().any(|b| b.hook.event == event)
    }

    fn matching<'a>(&'a self, event: &'a str, target: &'a str) -> impl Iterator<Item = &'a Bound> {
        self.hooks.iter().filter(move |b| {
            let m = b.hook.matcher.as_str();
            b.hook.event == event
                && (m.is_empty()
                    || m == "*"
                    || regex::Regex::new(&format!("^(?:{m})$")).is_ok_and(|re| re.is_match(target)))
        })
    }

    /// Run every matching hook in order; background ones aren't waited for.
    async fn fire(&self, event: &str, target: &str, extra: Value) -> Vec<Out> {
        let mut input = json!({
            "session_id": self.session,
            "cwd": self.cwd,
            "hook_event_name": event,
            "permission_mode": "default",
        });
        if let (Some(o), Some(e)) = (input.as_object_mut(), extra.as_object()) {
            o.extend(e.clone());
        }
        let default = match event {
            "SessionStart" | "SubagentStart" | "UserPromptSubmit" => 30,
            "SessionEnd" => 5,
            _ => 60,
        };
        let mut outs = Vec::new();
        for b in self.matching(event, target) {
            let timeout = Duration::from_secs(b.hook.timeout_secs.unwrap_or(default));
            if b.hook.background {
                let (cmd, cwd, input) = (self.command(b), self.cwd.clone(), input.clone());
                tokio::spawn(async move { run(cmd, &cwd, &input, timeout).await });
                continue;
            }
            if let Some(o) = run(self.command(b), &self.cwd, &input, timeout).await {
                outs.push(o);
            }
        }
        outs
    }

    fn command(&self, b: &Bound) -> tokio::process::Command {
        let project = self.cwd.display().to_string();
        let fill = |s: &str| b.parts.fill(s, true).replace("${CLAUDE_PROJECT_DIR}", &project);
        let mut c = match &b.hook.args {
            Some(args) => {
                let mut c = tokio::process::Command::new(fill(&b.hook.command));
                c.args(args.iter().map(|a| fill(a)));
                c
            }
            None => {
                let mut c = tokio::process::Command::new(if b.hook.bash { "bash" } else { "sh" });
                c.arg("-c").arg(fill(&b.hook.command));
                c
            }
        };
        c.envs(b.env.iter().map(|(k, v)| (k, v)))
            .env("CLAUDE_PROJECT_DIR", &project)
            .env(DISABLED_ENV, "1");
        c
    }

    /// `SessionStart` (or `SubagentStart` in a subagent): text for the model.
    pub async fn start(&self, source: &str) -> Vec<String> {
        let outs = match &self.subagent {
            Some(t) => {
                self.fire("SubagentStart", t, json!({ "agent_type": t, "agent_id": self.session }))
                    .await
            }
            None => self.fire("SessionStart", source, json!({ "source": source })).await,
        };
        outs.iter().filter_map(Out::context).collect()
    }

    /// `UserPromptSubmit`: text for the model, or why the prompt is refused.
    pub async fn prompt(&self, prompt: &str) -> Result<Vec<String>, String> {
        if self.subagent.is_some() {
            return Ok(vec![]);
        }
        let outs = self.fire("UserPromptSubmit", "", json!({ "prompt": prompt })).await;
        if let Some(why) = outs.iter().find_map(Out::block) {
            return Err(why);
        }
        Ok(outs.iter().filter_map(Out::context).collect())
    }

    /// `Stop`: the reason to keep going, when a hook blocks the stop.
    pub async fn stop(&self, active: bool, last: &str) -> Option<String> {
        if self.subagent.is_some() {
            return None;
        }
        let outs = self
            .fire(
                "Stop",
                "",
                json!({ "stop_hook_active": active, "last_assistant_message": last }),
            )
            .await;
        outs.iter().find_map(Out::block).filter(|r| !r.trim().is_empty())
    }

    pub async fn end(&self) {
        if self.subagent.is_none() {
            self.fire("SessionEnd", "", json!({ "reason": "other" })).await;
        }
    }
}

impl Handler for PluginHooks {
    fn label(&self) -> &str {
        "plugin hook"
    }

    fn before_tool<'a>(&'a self, tool: &'a str, args: &'a Value) -> BoxFuture<'a, Gate> {
        Box::pin(async move {
            let name = claude_tool(tool);
            if self.matching("PreToolUse", &name).next().is_none() {
                return Gate::Allow;
            }
            let outs = self
                .fire(
                    "PreToolUse",
                    &name,
                    json!({ "tool_name": name, "tool_input": claude_input(args) }),
                )
                .await;
            let mut new_args: Option<Value> = None;
            for o in &outs {
                let j = o.json().unwrap_or_default();
                let spec = &j["hookSpecificOutput"];
                if o.code == Some(2) || spec["permissionDecision"] == "deny" || j["decision"] == "block" {
                    let why = [
                        spec["permissionDecisionReason"].as_str(),
                        j["reason"].as_str(),
                        Some(o.stderr.trim()),
                    ]
                    .into_iter()
                    .flatten()
                    .find(|s| !s.is_empty())
                    .unwrap_or("refused by a plugin hook");
                    return Gate::Block(why.to_string());
                }
                if let Some(u) = spec["updatedInput"].as_object() {
                    let base = new_args.get_or_insert_with(|| args.clone());
                    if let Some(b) = base.as_object_mut() {
                        for (k, v) in u {
                            let k = if k == "file_path" { "path" } else { k.as_str() };
                            b.insert(k.to_string(), v.clone());
                        }
                    }
                }
            }
            match new_args {
                Some(a) => Gate::Args(a),
                None => Gate::Allow,
            }
        })
    }

    fn after_tool<'a>(
        &'a self,
        tool: &'a str,
        args: &'a Value,
        out: &'a ToolOutput,
        _run: &'a Run,
    ) -> BoxFuture<'a, Option<After>> {
        Box::pin(async move {
            let name = claude_tool(tool);
            self.matching("PostToolUse", &name).next()?;
            let text: String = out
                .content
                .iter()
                .filter_map(|b| match b {
                    novacode_types::Block::Text { text, .. } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n")
                .chars()
                .take(16_000)
                .collect();
            let outs = self
                .fire(
                    "PostToolUse",
                    &name,
                    json!({
                        "tool_name": name,
                        "tool_input": claude_input(args),
                        "tool_response": { "output": text, "is_error": out.is_error },
                    }),
                )
                .await;
            let mut after = After::default();
            let mut notes = Vec::new();
            for o in &outs {
                let j = o.json().unwrap_or_default();
                if let Some(r) = j["hookSpecificOutput"]["updatedToolOutput"].as_str() {
                    after.replace = Some(r.to_string());
                }
                notes.extend(o.block().filter(|s| !s.is_empty()));
                notes.extend(
                    j["hookSpecificOutput"]["additionalContext"]
                        .as_str()
                        .map(str::to_string),
                );
            }
            if !notes.is_empty() {
                after.append = Some(notes.join("\n"));
            }
            (after.append.is_some() || after.replace.is_some()).then_some(after)
        })
    }
}

/// Run one hook with `input` on stdin; `None` when it can't start or times out.
async fn run(mut c: tokio::process::Command, cwd: &Path, input: &Value, timeout: Duration) -> Option<Out> {
    c.current_dir(cwd)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .process_group(0)
        .kill_on_drop(true);
    let mut child = c
        .spawn()
        .map_err(|e| tracing::warn!("plugin hook did not start: {e}"))
        .ok()?;
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(input.to_string().as_bytes()).await;
    }
    match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Ok(Ok(o)) => Some(Out {
            code: o.status.code(),
            stdout: String::from_utf8_lossy(&o.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&o.stderr).into_owned(),
        }),
        Ok(Err(e)) => {
            tracing::warn!("plugin hook failed: {e}");
            None
        }
        Err(_) => {
            tracing::warn!("plugin hook timed out after {}s (fail-open)", timeout.as_secs());
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plugin(dir: &Path, hooks: Value) -> Package {
        let p = dir.join(".novacode/packages/p");
        std::fs::create_dir_all(p.join(".claude-plugin")).unwrap();
        std::fs::write(
            p.join(".claude-plugin/plugin.json"),
            json!({"name": "p", "hooks": hooks}).to_string(),
        )
        .unwrap();
        Package {
            name: "p".into(),
            dir: p,
            link: None,
            disabled: None,
        }
    }

    fn cmd(c: &str) -> Value {
        json!([{"hooks": [{"type": "command", "command": c}]}])
    }

    #[tokio::test]
    async fn events_in_claude_shape() {
        let t = tempfile::tempdir().unwrap();
        let p = plugin(
            t.path(),
            json!({
                "SessionStart": [{"matcher": "startup|compact", "hooks": [{"type": "command", "command": "echo \"ROOT=$CLAUDE_PLUGIN_ROOT\"; cat >/dev/null"}]}],
                "UserPromptSubmit": cmd(r#"jq -r 'if .prompt == "bad" then halt_error(2) else {hookSpecificOutput: {additionalContext: ("saw " + .prompt)}} end' 2>/dev/null || { echo nope >&2; exit 2; }"#),
                "PreToolUse": [{"matcher": "Bash", "hooks": [{"type": "command", "command": r#"jq -e '.tool_input.command | test("rm")' >/dev/null && { echo "no rm" >&2; exit 2; }; echo '{"hookSpecificOutput":{"updatedInput":{"command":"echo safe"}}}'"#}]}],
                "PostToolUse": [{"matcher": "Read", "hooks": [{"type": "command", "command": r#"jq -c '{hookSpecificOutput: {additionalContext: ("read " + .tool_input.file_path)}}'"#}]}],
                "Stop": cmd(r#"jq -e '.stop_hook_active' >/dev/null && exit 0; echo '{"decision":"block","reason":"run the tests"}'"#),
            }),
        );
        let h = PluginHooks::load(std::slice::from_ref(&p), t.path(), "s1", None);
        let start = h.start("startup").await;
        assert_eq!(start, vec![format!("ROOT={}", p.dir.display())]);
        assert!(h.start("resume").await.is_empty(), "matcher on the source");
        assert_eq!(h.prompt("hi").await.unwrap(), vec!["saw hi"]);
        assert!(h.prompt("bad").await.is_err());
        assert!(matches!(
            h.before_tool("bash", &json!({"command": "rm -rf x"})).await,
            Gate::Block(w) if w == "no rm"
        ));
        assert!(matches!(
            h.before_tool("bash", &json!({"command": "ls"})).await,
            Gate::Args(a) if a["command"] == "echo safe"
        ));
        assert!(matches!(
            h.before_tool("read", &json!({"path": "a"})).await,
            Gate::Allow
        ));
        let out = ToolOutput::text("x");
        let run = Run {
            call_id: "c".into(),
            duration_ms: 0,
            touched: vec![],
        };
        let a = h
            .after_tool("read", &json!({"path": "a.rs"}), &out, &run)
            .await
            .unwrap();
        assert_eq!(a.append.as_deref(), Some("read a.rs"));
        assert_eq!(h.stop(false, "done").await.as_deref(), Some("run the tests"));
        assert_eq!(h.stop(true, "done").await, None, "stop_hook_active ends the loop");
        let sub = PluginHooks::load(&[p], t.path(), "s1", Some("worker".into()));
        assert!(
            sub.prompt("bad").await.unwrap().is_empty(),
            "no prompt hooks in a subagent"
        );
    }

    #[test]
    fn tool_names() {
        assert_eq!(claude_tool("bash"), "Bash");
        assert_eq!(claude_tool("edit"), "Edit");
        assert_eq!(claude_tool("notes__search"), "mcp__notes__search");
        assert_eq!(claude_tool("codemode"), "codemode");
    }
}
