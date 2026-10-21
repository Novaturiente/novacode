//! Mode presets and plan mode (L10.7). A preset is a named bundle of a prompt
//! section plus tool rules (read-only, a tool list). Switching one appends a
//! system delta and refuses tools in the pipeline, so the cache holds (L4.7/L4.8).

use crate::policy::Policy;
use futures_util::future::BoxFuture;
use novacode_tools::ask::Asker;
use novacode_tools::{Tool, ToolCtx, ToolOutput};
use novacode_types::{Choice, Question};
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

pub const PLAN_PROMPT: &str = "Plan mode is on. You can read, search and run read-only commands; every change (edits, writes, mutating commands) is refused. Investigate the code the task touches, ask the user about real ambiguities, then call `plan_complete` with a decision-ready plan: the files to change and the change in each, the check that proves it works, and what you deliberately leave out. Don't start building until the plan is approved.";

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct Preset {
    #[serde(default)]
    pub name: String,
    /// Added as the `mode` prompt section while the preset is on.
    #[serde(default)]
    pub prompt: String,
    #[serde(default, rename = "readOnly")]
    pub read_only: bool,
    /// Only these tools run.
    #[serde(default)]
    pub tools: Option<Vec<String>>,
}

pub struct Modes {
    policy: Arc<Policy>,
    presets: Vec<Preset>,
    active: Mutex<Option<String>>,
}

impl Modes {
    /// `plan` built in; `modes.<name>` from settings add to or replace it.
    pub fn new(policy: Arc<Policy>, user: std::collections::BTreeMap<String, Preset>) -> Arc<Self> {
        let mut presets = vec![Preset {
            name: "plan".into(),
            prompt: PLAN_PROMPT.into(),
            read_only: true,
            tools: None,
        }];
        for (name, mut p) in user {
            p.name = name;
            presets.retain(|x| x.name != p.name);
            presets.push(p);
        }
        Arc::new(Modes {
            policy,
            presets,
            active: Mutex::default(),
        })
    }

    pub fn names(&self) -> Vec<String> {
        self.presets.iter().map(|p| p.name.clone()).collect()
    }

    pub fn active(&self) -> Option<String> {
        self.active.lock().unwrap().clone()
    }

    /// Turn a preset on (`None` = off). Tool rules apply at once; the prompt
    /// section from the next run.
    pub fn set(&self, name: Option<&str>) -> Result<(), String> {
        let p = match name {
            Some(n) => Some(
                self.presets
                    .iter()
                    .find(|p| p.name == n)
                    .ok_or_else(|| format!("no mode `{n}`; known: {}", self.names().join(", ")))?,
            ),
            None => None,
        };
        self.policy
            .set_overlay(p.map(|p| (p.name.clone(), p.read_only, p.tools.clone())));
        *self.active.lock().unwrap() = p.map(|p| p.name.clone());
        Ok(())
    }

    /// The `mode` prompt section of the active preset.
    pub fn prompt(&self) -> Option<String> {
        let a = self.active()?;
        let p = self.presets.iter().find(|p| p.name == a)?;
        Some(if p.prompt.is_empty() {
            format!("{a} mode is on.")
        } else {
            p.prompt.clone()
        })
    }
}

fn choice(label: &str, description: &str) -> Choice {
    serde_json::from_value(json!({"label": label, "description": description})).expect("choice")
}

/// The model asks to switch into plan mode; the user decides.
pub struct EnterPlanMode {
    pub modes: Arc<Modes>,
    pub asker: Arc<dyn Asker>,
}

impl Tool for EnterPlanMode {
    fn name(&self) -> &str {
        "enter_plan_mode"
    }
    fn description(&self) -> &str {
        "Ask the user to switch into read-only plan mode before non-trivial work: multi-file changes, refactors, migrations, architecture choices, or a request with more than one plausible reading. Skip it for small, clear fixes."
    }
    fn schema(&self) -> Value {
        json!({"type": "object", "properties": {"reason": {"type": "string", "description": "Why planning first helps, in one sentence."}}, "required": ["reason"]})
    }
    fn sequential(&self) -> bool {
        true
    }
    fn large_output(&self) -> bool {
        false
    }
    fn execute<'a>(&'a self, args: Value, _ctx: &'a ToolCtx) -> BoxFuture<'a, ToolOutput> {
        Box::pin(async move {
            if self.modes.active().as_deref() == Some("plan") {
                return ToolOutput::text("Plan mode is already on.");
            }
            let reason = args["reason"].as_str().unwrap_or_default();
            let q = Question {
                question: format!("Switch to plan mode? {reason}"),
                header: Some("Plan mode".into()),
                options: vec![
                    choice("Plan first", "Read-only until you approve a plan."),
                    choice("Just do it", "Skip planning; work as usual."),
                ],
                multi: false,
            };
            match self.asker.ask(vec![q]).await {
                Some(r) if r[0].chosen.first().is_some_and(|c| c == "Plan first") => match self.modes.set(Some("plan"))
                {
                    Ok(()) => ToolOutput::text(format!("Plan mode is on. {PLAN_PROMPT}")),
                    Err(e) => ToolOutput::error(e),
                },
                Some(r) => ToolOutput::text(format!(
                    "The user declined plan mode{}. Carry on without it.",
                    r[0].text.as_deref().map(|t| format!(": {t}")).unwrap_or_default()
                )),
                None => ToolOutput::text("The user dismissed the question; carry on without plan mode."),
            }
        })
    }
}

/// The plan is ready: the user approves it (plan mode ends) or asks for changes.
pub struct PlanComplete {
    pub modes: Arc<Modes>,
    pub asker: Arc<dyn Asker>,
}

impl Tool for PlanComplete {
    fn name(&self) -> &str {
        "plan_complete"
    }
    fn description(&self) -> &str {
        "In plan mode: submit the finished plan (markdown) for the user's approval. On approval plan mode ends and you build it."
    }
    fn schema(&self) -> Value {
        json!({"type": "object", "properties": {"plan": {"type": "string", "description": "The decision-ready plan in markdown."}}, "required": ["plan"]})
    }
    fn sequential(&self) -> bool {
        true
    }
    fn large_output(&self) -> bool {
        false
    }
    fn execute<'a>(&'a self, args: Value, _ctx: &'a ToolCtx) -> BoxFuture<'a, ToolOutput> {
        Box::pin(async move {
            if self.modes.active().as_deref() != Some("plan") {
                return ToolOutput::error("Plan mode is off; there's nothing to approve. Just do the work.");
            }
            let plan = args["plan"].as_str().unwrap_or_default();
            let mut approve = choice("Approve", "End plan mode and build it.");
            approve.preview = Some(plan.to_string());
            let q = Question {
                question: "Approve this plan?".into(),
                header: Some("Plan".into()),
                options: vec![approve, choice("Keep planning", "Type what to change.")],
                multi: false,
            };
            let edited = |r: &novacode_types::Reply| {
                r.preview
                    .as_ref()
                    .map(|p| format!("\n\nThe user edited the plan in their editor. This is the plan now:\n\n{p}"))
                    .unwrap_or_default()
            };
            match self.asker.ask(vec![q]).await {
                Some(r) if r[0].chosen.first().is_some_and(|c| c == "Approve") => {
                    let _ = self.modes.set(None);
                    let note = r[0]
                        .text
                        .as_ref()
                        .map(|t| format!(" The user adds: {t}"))
                        .unwrap_or_default();
                    ToolOutput::text(format!(
                        "The user approved the plan. Plan mode is off: build it now, exactly as approved.{note}{}",
                        edited(&r[0])
                    ))
                }
                Some(r) => ToolOutput::text(format!(
                    "Not approved; still in plan mode. The user says: {}{}",
                    r[0].text.as_deref().unwrap_or("revise the plan"),
                    edited(&r[0])
                )),
                None => ToolOutput::text("The user dismissed it; still in plan mode. Ask what they want changed."),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::{Permissions, Verdict};

    #[test]
    fn plan_mode_refuses_changes_until_off() {
        let policy = Arc::new(Policy::new(Permissions::default(), "/home/x".into(), None));
        let mut user = std::collections::BTreeMap::new();
        user.insert(
            "teach".to_string(),
            Preset {
                name: String::new(),
                prompt: "Explain as you go.".into(),
                read_only: false,
                tools: Some(vec!["read".into()]),
            },
        );
        let m = Modes::new(policy.clone(), user);
        let cwd = std::path::Path::new("/tmp");
        let edit = json!({"path": "/tmp/a"});
        assert_eq!(policy.check("edit", &edit, cwd, false), Verdict::Allow);
        m.set(Some("plan")).unwrap();
        assert!(
            matches!(policy.check("edit", &edit, cwd, false), Verdict::Deny(r) if r.contains("Plan mode is read-only"))
        );
        assert_eq!(policy.check("read", &edit, cwd, false), Verdict::Allow);
        assert!(m.prompt().unwrap().starts_with("Plan mode is on"));
        m.set(Some("teach")).unwrap();
        assert!(matches!(policy.check("grep", &json!({}), cwd, false), Verdict::Deny(r) if r.contains("teach mode")));
        m.set(None).unwrap();
        assert_eq!(policy.check("edit", &edit, cwd, false), Verdict::Allow);
        assert!(m.prompt().is_none() && m.set(Some("nope")).is_err());
    }
}
