//! The `skill` tool (L5.25): the model activates a skill by name. It returns the
//! skill's text and applies its Claude Code fields: `allowed-tools` run without
//! asking and `disallowed-tools` are refused, both until the next user message;
//! `context: fork` runs the skill as a subagent instead. Reading `SKILL.md`
//! directly still works, without those fields.

use crate::policy::{Policy, Rule};
use crate::resources::{self, Skill};
use futures_util::future::BoxFuture;
use novacode_tools::{Tool, ToolCtx, ToolOutput};
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::Arc;

/// Starts a subagent: (task, agent type, title) → a line for the model.
pub type Fork = Arc<dyn Fn(String, String, String) -> Result<String, String> + Send + Sync>;

pub struct SkillTool {
    /// Skill folders, read at each call so a new skill is found.
    pub dirs: Vec<PathBuf>,
    pub policy: Arc<Policy>,
    /// None where no subagent can start (depth limit, no window).
    pub fork: Option<Fork>,
    /// Where an activated skill's `hooks` go; none inside a hook's own novacode.
    pub hooks: Option<Arc<crate::plugin_hooks::PluginHooks>>,
}

/// A Claude Code permission rule as novacode's: tool names mapped, `Bash(git log *)` → `Bash(git log:*)`.
pub fn rule(claude: &str) -> Option<Rule> {
    let (tool, pattern) = match claude.split_once('(') {
        Some((t, rest)) => (t, Some(rest.strip_suffix(')')?)),
        None => (claude, None),
    };
    let tool = crate::plugin_hooks::TOOLS
        .iter()
        .find(|(c, _)| *c == tool.trim())
        .map_or(tool.trim(), |(_, n)| n);
    let pattern = pattern.map(|p| p.strip_suffix(" *").map_or(p.to_string(), |p| format!("{p}:*")));
    Rule::parse(&match pattern {
        Some(p) => format!("{tool}({p})"),
        None => tool.to_string(),
    })
}

fn tool_name(claude: &str) -> String {
    let t = claude.split_once('(').map_or(claude, |(t, _)| t).trim();
    crate::plugin_hooks::TOOLS
        .iter()
        .find(|(c, _)| *c == t)
        .map_or(t, |(_, n)| n)
        .to_string()
}

impl SkillTool {
    fn activate(&self, s: &Skill, args: &str) -> ToolOutput {
        let skills = [Skill {
            model_only: false,
            ..s.clone()
        }];
        let text = resources::expand(&format!("/skill:{} {args}", s.name), &skills, &[]);
        // Its hooks run for the rest of the session, forked or not (as in Claude Code).
        let skipped = self.hooks.as_ref().map(|h| h.add_skill(s)).unwrap_or_default();
        if !skipped.is_empty() {
            tracing::warn!(skill = %s.name, ?skipped, "skill hooks not run");
        }
        // Without a subagent here, the skill is followed in this conversation instead.
        if let (Some(agent), Some(fork)) = (&s.fork, &self.fork) {
            return match fork(text, agent.clone(), format!("skill {}", s.name)) {
                Ok(line) => ToolOutput::text(format!("Skill `{}` runs as a subagent: {line}", s.name)),
                Err(e) => ToolOutput::error(format!("skill `{}`: {e}", s.name)),
            };
        }
        self.policy.for_turn(
            s.allowed_tools.iter().filter_map(|t| rule(t)).collect(),
            s.disallowed_tools.iter().map(|t| tool_name(t)).collect(),
        );
        let mut notes = Vec::new();
        if !s.allowed_tools.is_empty() {
            notes.push(format!("run without asking: {}", s.allowed_tools.join(", ")));
        }
        if !s.disallowed_tools.is_empty() {
            notes.push(format!("off: {}", s.disallowed_tools.join(", ")));
        }
        let head = if notes.is_empty() {
            String::new()
        } else {
            format!("Until the user's next message: {}.\n\n", notes.join("; "))
        };
        ToolOutput::text(format!("{head}{text}"))
    }
}

impl Tool for SkillTool {
    fn name(&self) -> &str {
        "skill"
    }
    fn description(&self) -> &str {
        "Activate a skill listed in the system prompt by name: returns its instructions and applies its tool rules for this turn (or runs it as a subagent when the skill says so). Follow the instructions it returns."
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "name": {"type": "string", "description": "The skill's name as listed."},
                "args": {"type": "string", "description": "Arguments for the skill, if it takes any."}
            },
            "required": ["name"]
        })
    }
    fn large_output(&self) -> bool {
        false
    }
    fn execute<'a>(&'a self, args: Value, _ctx: &'a ToolCtx) -> BoxFuture<'a, ToolOutput> {
        Box::pin(async move {
            let name = args["name"].as_str().unwrap_or_default().trim().trim_start_matches('/');
            let skills = resources::skills(&self.dirs).0;
            let Some(s) = skills.iter().find(|s| s.name == name) else {
                let names: Vec<&str> = skills.iter().filter(|s| !s.hidden).map(|s| s.name.as_str()).collect();
                return ToolOutput::error(format!("no skill `{name}`; skills: {}", names.join(", ")));
            };
            if s.hidden {
                return ToolOutput::error(format!("skill `{name}` runs only when the user types /skill:{name}"));
            }
            self.activate(s, args["args"].as_str().unwrap_or_default())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::{Mode, Permissions, Verdict};
    use std::path::Path;

    #[test]
    fn claude_rules_map_to_novacode_rules() {
        assert_eq!(rule("Bash(git log *)").unwrap().to_string(), "Bash(git log:*)");
        assert_eq!(rule("Read").unwrap().to_string(), "Read");
        assert_eq!(rule("Glob").unwrap().to_string(), "Find");
        assert_eq!(tool_name("MultiEdit"), "edit");
    }

    #[tokio::test]
    async fn activating_applies_the_turn_rules_and_forks() {
        let dir = tempfile::tempdir().unwrap();
        let mk = |name: &str, fm: &str| {
            let d = dir.path().join(name);
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(
                d.join("SKILL.md"),
                format!("---\nname: {name}\ndescription: d\n{fm}---\nDo it for $ARGUMENTS."),
            )
            .unwrap();
        };
        mk(
            "commit",
            "allowed-tools: Bash(git status *) Bash(git commit *)\ndisallowed-tools: [Write]\n",
        );
        mk("deploy", "context: fork\nagent: worker\n");
        mk("manual", "disable-model-invocation: true\n");
        let policy = Arc::new(Policy::new(Permissions::default(), "/h".into(), None).with_mode(Mode::Ask));
        let started = Arc::new(std::sync::Mutex::new(Vec::new()));
        let s2 = started.clone();
        let tool = SkillTool {
            dirs: vec![dir.path().to_path_buf()],
            policy: policy.clone(),
            fork: Some(Arc::new(move |task, agent, _| {
                s2.lock().unwrap().push((task, agent));
                Ok("a1".into())
            })),
            hooks: None,
        };
        let ctx = ToolCtx {
            cwd: dir.path().into(),
            env: Arc::new(novacode_tools::LocalEnv),
            cancel: Default::default(),
            updates: None,
            files: Default::default(),
            session_id: "t".into(),
            processes: None,
            native_tools: false,
            nested: None,
            script: false,
        };
        let text = |o: &ToolOutput| format!("{:?}", o.content);
        let bash = |c: &str| policy.check("bash", &json!({"command": c}), Path::new("/w"), true);
        assert!(matches!(bash("git commit -m x"), Verdict::Ask { .. }));
        let out = tool.execute(json!({"name": "commit", "args": "main"}), &ctx).await;
        assert!(
            !out.is_error && text(&out).contains("Do it for main."),
            "{}",
            text(&out)
        );
        assert_eq!(bash("git commit -m x"), Verdict::Allow, "allowed-tools for this turn");
        assert!(matches!(bash("git push"), Verdict::Ask { .. }));
        assert!(matches!(
            policy.check("write", &json!({"path": "a"}), Path::new("/w"), true),
            Verdict::Deny(_)
        ));
        policy.end_turn();
        assert!(
            matches!(bash("git commit -m x"), Verdict::Ask { .. }),
            "the next message ends them"
        );
        assert!(matches!(
            policy.check("write", &json!({"path": "a"}), Path::new("/w"), true),
            Verdict::Ask { .. }
        ));
        // `context: fork` starts a subagent with the skill's text.
        let out = tool.execute(json!({"name": "deploy"}), &ctx).await;
        assert!(text(&out).contains("runs as a subagent: a1"));
        let got = started.lock().unwrap().clone();
        assert_eq!(got[0].1, "worker");
        assert!(got[0].0.contains("<skill name=\"deploy\""));
        assert!(tool.execute(json!({"name": "manual"}), &ctx).await.is_error);
    }
}
