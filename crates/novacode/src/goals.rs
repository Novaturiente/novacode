//! Goals (`/goal`): an objective with checkable completion criteria that the
//! agent keeps working on by itself until it is met.
//!
//! Built for novacode rather than copied from pi-goal-x:
//! - The plan is the existing `todo` list; a goal adds only the objective,
//!   the criteria and the loop.
//! - Continuation is one saved hidden reminder per settled run (cache-safe),
//!   never a per-request injection.
//! - The loop stops itself: Esc pauses, two runs in a row without a tool call
//!   pause as stalled, and `goal.maxRuns` caps automatic runs per start/resume.
//! - The completion auditor is the `auditor` subagent: a separate process with
//!   read-only tools that re-checks every criterion in the workspace.
//! - State is one JSON file per session, so pause/resume/audit toggles survive
//!   a restart without touching the conversation.

use crate::agents::Supervisor;
use futures_util::future::BoxFuture;
use novacode_base::storage::{self, Durability};
use novacode_core::agent::SessionHandle;
use novacode_tools::{Tool, ToolCtx, ToolOutput};
use novacode_types::ui::{Node, Time, UiOp};
use novacode_types::{AgentEvent, StopReason};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    Active,
    /// Needs the user (the agent called `block`).
    Blocked,
    /// Stopped by the user, a stall or the run limit; `/goal resume` continues.
    Paused,
    Auditing,
    Done,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Goal {
    pub objective: String,
    pub criteria: Vec<String>,
    /// Steps (the todo list) are done in order, one per run.
    #[serde(default)]
    pub ordered: bool,
    pub state: State,
    /// Why it's blocked or paused, or the auditor's feedback.
    #[serde(default)]
    pub note: Option<String>,
    pub audit: bool,
    pub started_ms: u64,
    #[serde(default)]
    pub ended_ms: Option<u64>,
    /// Automatic continuations since it started or was resumed.
    #[serde(default)]
    pub runs: u32,
    #[serde(default)]
    stalls: u32,
}

pub struct Goals {
    path: PathBuf,
    goal: Mutex<Option<Goal>>,
    sup: Option<Arc<Supervisor>>,
    max_runs: u32,
    audit: bool,
    audit_model: Option<String>,
    handle: OnceLock<SessionHandle>,
}

impl Goals {
    pub fn new(
        path: PathBuf,
        sup: Option<Arc<Supervisor>>,
        max_runs: u32,
        audit: bool,
        audit_model: Option<String>,
    ) -> Arc<Self> {
        let goal = storage::read_json::<Goal>(&path).ok().flatten();
        Arc::new(Goals {
            path,
            goal: Mutex::new(goal),
            sup,
            max_runs,
            audit,
            audit_model,
            handle: OnceLock::new(),
        })
    }

    pub fn current(&self) -> Option<Goal> {
        self.goal.lock().unwrap().clone()
    }

    /// Change the goal, save it and show it in the Agent tab.
    fn update<R>(&self, f: impl FnOnce(&mut Option<Goal>) -> R) -> R {
        let mut g = self.goal.lock().unwrap();
        let r = f(&mut g);
        let saved = match g.as_ref() {
            Some(goal) => {
                let _ = std::fs::create_dir_all(self.path.parent().unwrap_or(&self.path));
                storage::write_json(&self.path, goal, Durability::Fast)
            }
            None => std::fs::remove_file(&self.path).or_else(|e| match e.kind() {
                std::io::ErrorKind::NotFound => Ok(()),
                _ => Err(e),
            }),
        };
        if let Err(e) = saved {
            tracing::warn!(%e, "goal state");
        }
        let node = g.as_ref().map(|g| self.node(g));
        drop(g);
        self.publish(node);
        r
    }

    fn node(&self, g: &Goal) -> Node {
        let status = match g.state {
            State::Active => "running",
            State::Auditing | State::Blocked => "waiting",
            State::Paused => "stopped",
            State::Done => "done",
        };
        let mut detail: Vec<String> = g.criteria.iter().map(|c| format!("· {c}")).collect();
        let what = match g.state {
            State::Active => format!("working · run {}/{}", g.runs, self.max_runs),
            State::Auditing => "the auditor is checking".into(),
            State::Blocked => "blocked: needs you".into(),
            State::Paused => "paused · /goal resume".into(),
            State::Done => "done".into(),
        };
        detail.push(match &g.note {
            Some(n) => format!("{what}: {}", n.lines().next().unwrap_or("")),
            None => what,
        });
        Node {
            id: "goal".into(),
            parent: None,
            label: g.objective.clone(),
            tag: None,
            status: Some(status.into()),
            icon: None,
            dim: g.state == State::Done,
            detail,
            time: Some(match g.ended_ms {
                Some(e) => Time {
                    ms: Some(e.saturating_sub(g.started_ms)),
                    since: None,
                },
                None => Time {
                    ms: None,
                    since: Some(g.started_ms),
                },
            }),
            order: Some(0.0),
            actions: Default::default(),
        }
    }

    fn publish(&self, node: Option<Node>) {
        let Some(h) = self.handle.get() else { return };
        let op = match node {
            Some(node) => UiOp::Publish {
                tab: "agent".into(),
                source: "goal".into(),
                node: Box::new(node),
            },
            None => UiOp::Remove {
                tab: "agent".into(),
                source: "goal".into(),
                id: None,
            },
        };
        h.emit(AgentEvent::Ui { ext: "goal".into(), op });
    }

    fn notice(&self, text: String) {
        if let Some(h) = self.handle.get() {
            h.emit(AgentEvent::Stream(novacode_types::StreamEvent::Notice { text }));
        }
    }

    /// Keep the goal going: after each settled run, a saved reminder wakes the
    /// agent again until the goal is done, blocked, paused or out of runs.
    pub fn watch(self: Arc<Self>, handle: SessionHandle) -> tokio::task::JoinHandle<()> {
        let mut rx = handle.subscribe();
        let _ = self.handle.set(handle.clone());
        let node = self.current().map(|g| self.node(&g));
        self.publish(node);
        tokio::spawn(async move {
            let mut tools = 0u32;
            loop {
                let ev = match rx.recv().await {
                    Ok(ev) => ev,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(_) => return,
                };
                match ev {
                    AgentEvent::AgentStart => tools = 0,
                    AgentEvent::ToolStart { .. } => tools += 1,
                    AgentEvent::AgentEnd {
                        stop: StopReason::Aborted,
                    } => {
                        if self.pause_if_active("you interrupted it") {
                            self.notice("goal paused: you interrupted it (/goal resume continues)".into());
                        }
                    }
                    AgentEvent::Settled => {
                        if let Some(text) = self.next_step(tools) {
                            handle.remind_as("goal", text, true);
                        }
                    }
                    _ => {}
                }
            }
        })
    }

    fn pause_if_active(&self, why: &str) -> bool {
        self.update(|g| match g {
            Some(g) if g.state == State::Active => {
                g.state = State::Paused;
                g.note = Some(why.into());
                true
            }
            _ => false,
        })
    }

    /// After a settled run: the continuation text, or `None` (and maybe a pause).
    fn next_step(&self, tools: u32) -> Option<String> {
        let (text, pause) = self.update(|g| {
            let g = g.as_mut().filter(|g| g.state == State::Active)?;
            g.stalls = if tools == 0 { g.stalls + 1 } else { 0 };
            let why = if g.stalls >= 2 {
                Some("stalled: two runs in a row without a tool call".to_string())
            } else if g.runs >= self.max_runs {
                Some(format!("reached goal.maxRuns ({})", self.max_runs))
            } else {
                None
            };
            if let Some(w) = why {
                g.state = State::Paused;
                g.note = Some(w.clone());
                return Some((None, Some(w)));
            }
            g.runs += 1;
            Some((Some(continuation(g)), None))
        })?;
        if let Some(w) = pause {
            self.notice(format!("goal paused: {w} (/goal resume continues)"));
        }
        text
    }

    /// `/goal pause|resume|clear|audit on|off`, or the status text for `/goal`.
    pub fn command(&self, arg: &str) -> Result<String, String> {
        match arg {
            "" | "status" => Ok(self
                .current()
                .map_or("no goal; /goal <what you want done> starts one".into(), |g| status(&g))),
            "pause" => self.update(|g| match g {
                Some(g) if matches!(g.state, State::Active | State::Blocked) => {
                    g.state = State::Paused;
                    g.note = Some("paused by you".into());
                    Ok("goal paused".into())
                }
                _ => Err("no running goal".into()),
            }),
            "resume" => {
                let text = self.update(|g| match g {
                    Some(g) if matches!(g.state, State::Paused | State::Blocked) => {
                        g.state = State::Active;
                        g.note = None;
                        g.runs = 0;
                        g.stalls = 0;
                        Ok(continuation(g))
                    }
                    _ => Err("no paused or blocked goal".to_string()),
                })?;
                if let Some(h) = self.handle.get() {
                    h.remind_as("goal", text, true);
                }
                Ok("goal resumed".into())
            }
            "clear" => {
                self.update(|g| *g = None);
                Ok("goal cleared".into())
            }
            "audit on" | "audit off" => self.update(|g| match g {
                Some(g) => {
                    g.audit = arg == "audit on";
                    Ok(format!("completion audit {}", if g.audit { "on" } else { "off" }))
                }
                None => Err("no goal".into()),
            }),
            _ => Err("unknown: use /goal, /goal <objective>, pause, resume, clear, audit on|off".into()),
        }
    }
}

fn continuation(g: &Goal) -> String {
    let step = if g.ordered {
        "Do the next pending step of the plan, in order"
    } else {
        "Continue with the next step"
    };
    let mut s = format!("Goal still open: {}\n{step}.", g.objective);
    if !g.criteria.is_empty() {
        s.push_str("\nDone when:");
        for c in &g.criteria {
            s.push_str(&format!("\n- {c}"));
        }
    }
    s.push_str("\nWhen every criterion is met, call `goal` with action `complete` and your evidence. If you need the user, call `goal` with action `block` and say what you need.");
    if let Some(n) = &g.note {
        s.push_str(&format!("\nLast audit said: {n}"));
    }
    s
}

fn status(g: &Goal) -> String {
    let mut s = format!("Goal: {}\nState: {:?}", g.objective, g.state);
    if g.ordered {
        s.push_str(" (ordered)");
    }
    for c in &g.criteria {
        s.push_str(&format!("\n  · {c}"));
    }
    if let Some(n) = &g.note {
        s.push_str(&format!("\nNote: {n}"));
    }
    s.push_str(&format!(
        "\nAutomatic runs: {} · audit {}",
        g.runs,
        if g.audit { "on" } else { "off" }
    ));
    s
}

/// `/goal <text>`: what the model is asked to do before calling `goal set`.
pub fn draft_prompt() -> &'static str {
    "The user's next message sets a goal. Draft one clear objective and 1-5 completion criteria that can be checked in the workspace (a test passes, a command prints X, a file has Y). If something essential is ambiguous, ask with ask_user first; otherwise don't ask. Then call `goal` with action `set`, put the steps in `todo`, and start working."
}

#[derive(Deserialize)]
#[serde(rename_all = "lowercase", tag = "action")]
enum Args {
    /// Start a goal (replaces the current one).
    Set {
        objective: String,
        /// Checkable completion criteria.
        criteria: Vec<String>,
        /// The todo steps must be done in order, one per run.
        #[serde(default)]
        ordered: bool,
    },
    /// Every criterion is met: give the evidence (commands run and their results).
    Complete {
        evidence: String,
    },
    /// You need the user to continue.
    Block {
        reason: String,
    },
    Status,
}

pub struct GoalTool(pub Arc<Goals>);

impl Tool for GoalTool {
    fn name(&self) -> &str {
        "goal"
    }
    fn description(&self) -> &str {
        "Work toward a goal until it is done. `set` starts one (objective + checkable criteria); while it is open you are woken after each turn to continue. `complete` with evidence when every criterion is met (an independent auditor may check it); `block` when you need the user; `status` shows it."
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": { "type": "string", "enum": ["set", "complete", "block", "status"] },
                "objective": { "type": "string", "description": "set: what is to be achieved" },
                "criteria": { "type": "array", "items": { "type": "string" }, "description": "set: checkable completion criteria" },
                "ordered": { "type": "boolean", "description": "set: the todo steps must be done in order, one per run" },
                "evidence": { "type": "string", "description": "complete: commands run and their results, per criterion" },
                "reason": { "type": "string", "description": "block: what you need from the user" }
            },
            "required": ["action"]
        })
    }
    fn sequential(&self) -> bool {
        true
    }
    fn guidelines(&self) -> &[&str] {
        &[
            "Only start a `goal` when the user asks for one (/goal). Never call `goal` complete without having run the checks the criteria name.",
        ]
    }
    fn execute<'a>(&'a self, args: Value, _ctx: &'a ToolCtx) -> BoxFuture<'a, ToolOutput> {
        Box::pin(async move {
            let a: Args = match novacode_tools::parse(args) {
                Ok(a) => a,
                Err(e) => return e,
            };
            let g = &self.0;
            match a {
                Args::Set {
                    objective,
                    criteria,
                    ordered,
                } => {
                    let goal = Goal {
                        objective,
                        criteria,
                        ordered,
                        state: State::Active,
                        note: None,
                        audit: g.audit && g.sup.is_some(),
                        started_ms: novacode_core::session::now_ms(),
                        ended_ms: None,
                        runs: 0,
                        stalls: 0,
                    };
                    let text = format!(
                        "goal set (audit {}). Work on it now; you'll be woken to continue after each turn.",
                        if goal.audit { "on" } else { "off" }
                    );
                    g.update(|cur| *cur = Some(goal));
                    ToolOutput::text(text)
                }
                Args::Block { reason } => {
                    let ok = g.update(|cur| match cur {
                        Some(x) if x.state == State::Active => {
                            x.state = State::Blocked;
                            x.note = Some(reason.clone());
                            true
                        }
                        _ => false,
                    });
                    if !ok {
                        return ToolOutput::error("no active goal");
                    }
                    g.notice(format!("goal blocked: {reason} (/goal resume when it's sorted)"));
                    ToolOutput::text("goal blocked; the user has been told. Stop here and explain what you need.")
                }
                Args::Status => ToolOutput::text(g.current().map_or("no goal".into(), |x| status(&x))),
                Args::Complete { evidence } => {
                    let Some(goal) = g.current().filter(|x| x.state == State::Active) else {
                        return ToolOutput::error("no active goal");
                    };
                    let finish = |note: Option<String>| {
                        g.update(|cur| {
                            if let Some(x) = cur {
                                x.state = State::Done;
                                x.note = note;
                                x.ended_ms = Some(novacode_core::session::now_ms());
                            }
                        })
                    };
                    let Some(sup) = g.sup.as_ref().filter(|_| goal.audit) else {
                        finish(None);
                        return ToolOutput::text("goal done.");
                    };
                    g.update(|cur| {
                        if let Some(x) = cur {
                            x.state = State::Auditing;
                        }
                    });
                    let task = format!(
                        "Goal: {}\n\nCompletion criteria:\n{}\n\nThe agent's evidence:\n{evidence}",
                        goal.objective,
                        goal.criteria
                            .iter()
                            .map(|c| format!("- {c}"))
                            .collect::<Vec<_>>()
                            .join("\n")
                    );
                    let verdict = sup.audit(task, "audit the goal".into(), g.audit_model.clone()).await;
                    match verdict {
                        Ok(r) if approved(&r) => {
                            finish(Some("approved by the auditor".into()));
                            ToolOutput::text(format!("goal done: the auditor approved it.\n\n{r}"))
                        }
                        Ok(r) => {
                            let feedback = r.lines().skip(1).collect::<Vec<_>>().join("\n").trim().to_string();
                            g.update(|cur| {
                                if let Some(x) = cur {
                                    x.state = State::Active;
                                    x.note = Some(feedback.clone());
                                }
                            });
                            ToolOutput::error(format!(
                                "the auditor rejected completion; keep working on what it found:\n\n{feedback}"
                            ))
                        }
                        Err(e) => {
                            g.update(|cur| {
                                if let Some(x) = cur {
                                    x.state = State::Active;
                                }
                            });
                            ToolOutput::error(format!(
                                "the audit couldn't run ({e}); the goal stays open. Try `complete` again, or the user can turn the audit off with /goal audit off."
                            ))
                        }
                    }
                }
            }
        })
    }
}

fn approved(report: &str) -> bool {
    report.lines().find(|l| !l.trim().is_empty()).is_some_and(|l| {
        l.trim()
            .trim_matches('*')
            .trim()
            .eq_ignore_ascii_case("VERDICT: approved")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn goals(dir: &std::path::Path) -> Arc<Goals> {
        Goals::new(dir.join("g.json"), None, 3, true, None)
    }

    #[test]
    fn loop_continues_stalls_and_caps() {
        let d = std::env::temp_dir().join(format!("nc-goal-{}", std::process::id()));
        let g = goals(&d);
        g.update(|x| {
            *x = Some(Goal {
                objective: "ship it".into(),
                criteria: vec!["tests pass".into()],
                ordered: false,
                state: State::Active,
                note: None,
                audit: false,
                started_ms: 0,
                ended_ms: None,
                runs: 0,
                stalls: 0,
            })
        });
        let t = g.next_step(2).unwrap();
        assert!(t.contains("ship it") && t.contains("- tests pass"), "{t}");
        assert!(g.next_step(0).is_some(), "one quiet run is fine");
        assert!(g.next_step(0).is_none(), "two in a row: stalled");
        assert_eq!(g.current().unwrap().state, State::Paused);
        // State survives a restart; resume resets the counters.
        let g2 = goals(&d);
        assert_eq!(g2.current().unwrap().state, State::Paused);
        g2.command("resume").unwrap();
        for _ in 0..3 {
            assert!(g2.next_step(1).is_some());
        }
        assert!(g2.next_step(1).is_none(), "goal.maxRuns");
        assert!(g2.current().unwrap().note.unwrap().contains("maxRuns"));
        g2.command("clear").unwrap();
        assert!(goals(&d).current().is_none());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn verdict_line() {
        assert!(approved("\nVERDICT: approved\n- ok"));
        assert!(approved("**VERDICT: approved**"));
        assert!(!approved("VERDICT: rejected\nVERDICT: approved"));
    }
}
