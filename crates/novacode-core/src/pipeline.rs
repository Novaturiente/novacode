//! The tool pipeline (L3.2): lookup with a name suggestion, argument repair
//! and lenient coercion, permission check (the agent's preflight, L6.1),
//! execute, guard and redact the output once. Every call path uses it.
// Hooks (L7.1) run before the permission check and after the guard: agent::run_tools and codemode::Runner.

use novacode_base::redact::Redactor;

use novacode_tools::guard::{self, ACCEPT_LARGE, Budget};
use novacode_tools::{Tool, ToolCtx, ToolOutput};
use novacode_types::ToolCall;
use serde_json::Value;
use std::path::Path;
use std::sync::Arc;

/// Why a call failed before running; counted toward the malformed-round cap (L3.4).
pub enum Prepared {
    Ready(Arc<dyn Tool>, Value, bool),
    Rejected(ToolOutput),
}

pub fn prepare(tools: &[Arc<dyn Tool>], call: &ToolCall) -> Prepared {
    let Some(tool) = tools.iter().find(|t| t.name() == call.name) else {
        let hint = suggest(tools, &call.name)
            .map(|n| format!(" Did you mean `{n}`?"))
            .unwrap_or_default();
        let names: Vec<&str> = tools.iter().map(|t| t.name()).collect();
        return Prepared::Rejected(ToolOutput::error(format!(
            "Unknown tool `{}`.{hint} Available: {}.",
            call.name,
            names.join(", ")
        )));
    };
    if let Some(raw) = &call.arguments_error {
        return Prepared::Rejected(ToolOutput::error(format!(
            "The arguments were not a valid JSON object, so the tool did not run. Call it again with valid JSON. You sent: {raw}"
        )));
    }
    let mut args = call.arguments.clone();
    let accept = args
        .as_object_mut()
        .and_then(|o| o.remove(ACCEPT_LARGE))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    coerce(&mut args, &tool.schema());
    Prepared::Ready(tool.clone(), args, accept)
}

/// Schema sent to the model: the tool's own, plus `accept_large_output` where it applies.
/// The definition sent to a model: name, description, schema (with the guard field).
pub fn def(tool: &dyn Tool) -> novacode_types::ToolDef {
    novacode_types::ToolDef {
        name: tool.name().into(),
        description: tool.description().into(),
        schema: schema(tool),
    }
}

pub fn schema(tool: &dyn Tool) -> Value {
    let mut s = tool.schema();
    if tool.large_output()
        && let Some(props) = s.get_mut("properties").and_then(|p| p.as_object_mut())
    {
        props.insert(
            ACCEPT_LARGE.into(),
            serde_json::json!({"type": "boolean", "description": "Set true to receive output that was refused as too large."}),
        );
    }
    s
}

/// Lenient fixes for common model slips: "5" for 5, "true" for true, a JSON string for an array/object.
fn coerce(args: &mut Value, schema: &Value) {
    let (Some(obj), Some(props)) = (
        args.as_object_mut(),
        schema.get("properties").and_then(|p| p.as_object()),
    ) else {
        return;
    };
    for (k, v) in obj.iter_mut() {
        let Some(s) = v.as_str() else { continue };
        let ty = props.get(k).and_then(|p| p.get("type"));
        let wants = |t: &str| ty.is_some_and(|ty| ty == t || ty.as_array().is_some_and(|a| a.iter().any(|x| x == t)));
        let fixed = if wants("integer") || wants("number") {
            serde_json::from_str::<serde_json::Number>(s.trim())
                .ok()
                .map(Value::Number)
        } else if wants("boolean") {
            s.trim().parse::<bool>().ok().map(Value::Bool)
        } else if wants("array") || wants("object") {
            serde_json::from_str::<Value>(s)
                .ok()
                .filter(|p| p.is_array() || p.is_object())
        } else {
            None
        };
        if let Some(f) = fixed {
            *v = f;
        }
    }
}

fn suggest<'a>(tools: &'a [Arc<dyn Tool>], name: &str) -> Option<&'a str> {
    let lower = name.to_lowercase();
    tools
        .iter()
        .map(|t| (t.name(), edit_distance(&lower, &t.name().to_lowercase())))
        .filter(|(_, d)| *d <= 3)
        .min_by_key(|(_, d)| *d)
        .map(|(n, _)| n)
}

fn edit_distance(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut cur = vec![i + 1];
        for (j, cb) in b.iter().enumerate() {
            cur.push((prev[j] + usize::from(ca != *cb)).min(prev[j + 1] + 1).min(cur[j] + 1));
        }
        prev = cur;
    }
    prev[b.len()]
}

/// Permission check for one call (L6.1). `None` = run it; `Some` = the refusal to return.
/// `read_only`: the tool declares it only reads ([`Tool::read_only`]).
pub async fn authorize(
    policy: &crate::policy::Policy,
    approver: Option<&Arc<dyn crate::policy::Approver>>,
    tool: &str,
    read_only: bool,
    args: &Value,
    cwd: &Path,
    hook: &crate::policy::HookDecision,
) -> Option<ToolOutput> {
    let never = tokio_util::sync::CancellationToken::new();
    authorize_cancellable(policy, approver, None, tool, read_only, args, cwd, hook, &never).await
}

/// [`authorize`], but a cancelled `cancel` ends a pending approval as a deny ("aborted"), not recorded.
/// `judge`: the command judge (L6.8) looks at risky commands the gate allowed.
#[allow(clippy::too_many_arguments)]
pub async fn authorize_cancellable(
    policy: &crate::policy::Policy,
    approver: Option<&Arc<dyn crate::policy::Approver>>,
    judge: Option<&crate::judge::Judge>,
    tool: &str,
    read_only: bool,
    args: &Value,
    cwd: &Path,
    hook: &crate::policy::HookDecision,
    cancel: &tokio_util::sync::CancellationToken,
) -> Option<ToolOutput> {
    use crate::judge::Decision;
    use crate::policy::{Approval, ApprovalRequest, Verdict};
    let (reason, rule, risky) = match policy.check_tool(tool, read_only, args, cwd, approver.is_some(), hook) {
        Verdict::Allow => {
            let judge = judge?;
            let (command, why) = crate::judge::scope(policy, tool, args, cwd)?;
            match judge.decide(&command, &why, cancel).await {
                Decision::Allow => return None,
                Decision::Aborted => return Some(ToolOutput::error("Not run: aborted.")),
                Decision::Block(reason) if approver.is_none() => {
                    return Some(ToolOutput::error(format!(
                        "Refused by the command judge: {reason}. Do it another way, or ask the user to run it."
                    )));
                }
                // Risky: no "always"; a yes is remembered for this exact command.
                Decision::Block(reason) => (format!("the command judge: {reason}"), command, true),
            }
        }
        Verdict::Deny(why) => return Some(ToolOutput::error(why)),
        Verdict::Ask { reason, rule, risky } => (reason, rule, risky),
    };
    let Some(approver) = approver else {
        return Some(ToolOutput::error(format!(
            "Not run: this needs the user's approval ({reason}) and nobody can approve it here. Add an allow rule or run interactively."
        )));
    };
    let req = ApprovalRequest {
        tool: tool.to_string(),
        args: args.clone(),
        reason,
        rule,
        risky,
    };
    let answer = tokio::select! {
        biased;
        _ = cancel.cancelled() => return Some(ToolOutput::error("Not run: aborted.")),
        a = approver.approve(req.clone()) => a,
    };
    policy.record(&req, &answer);
    match answer {
        Approval::Deny(why) => Some(ToolOutput::error(match why {
            Some(w) if !w.trim().is_empty() => format!("The user declined this call: {w}"),
            _ => "The user declined this call.".into(),
        })),
        _ => None,
    }
}

/// Execute and guard once (L5.3). Output text is cut/spilled, then budget-checked.
pub async fn run(
    tool: &dyn Tool,
    args: Value,
    accept: bool,
    ctx: &ToolCtx,
    spill: &Path,
    budget: Budget,
    redactor: &Redactor,
) -> ToolOutput {
    let mut out = tool.execute(args, ctx).await;
    for b in &mut out.content {
        if let novacode_types::Block::Text { text, .. } = b {
            // Redact before the spill file is written, so secrets never land there either.
            if let Some(clean) = redactor.apply(text) {
                *text = clean;
            }
            // A script's own output is cut when it reaches the model.
            if ctx.script {
                continue;
            }
            let cut = guard::truncate(text, tool.keep(), spill);
            *text = match guard::check_budget(&cut, budget, accept) {
                Ok(t) => t,
                Err(refusal) => {
                    out.is_error = true;
                    refusal
                }
            };
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A hook's ask with nobody to answer (print mode) is refused, not run.
    #[tokio::test]
    async fn hook_ask_is_refused_unattended() {
        use crate::policy::{HookDecision, Permissions, Policy};
        let p = Policy::new(Permissions::default(), "/h".into(), None);
        let args = json!({"path": "a"});
        let ask = HookDecision::Ask("check".into());
        let out = authorize(&p, None, "read", false, &args, Path::new("/w"), &ask).await;
        assert!(out.is_some_and(|o| o.is_error));
        assert!(
            authorize(&p, None, "read", false, &args, Path::new("/w"), &HookDecision::None)
                .await
                .is_none()
        );
    }

    mod judged {
        use super::*;
        use crate::judge::Judge;
        use crate::policy::{Approval, ApprovalRequest, Approver, HookDecision, Policy};
        use futures_util::future::BoxFuture;
        use novacode_ai::faux::{Faux, FauxReply};
        use std::sync::Mutex;

        /// Answers every card with `answer` and keeps the requests.
        struct Cards(Approval, Mutex<Vec<ApprovalRequest>>);
        impl Approver for Cards {
            fn approve(&self, req: ApprovalRequest) -> BoxFuture<'_, Approval> {
                self.1.lock().unwrap().push(req);
                let a = self.0.clone();
                Box::pin(async move { a })
            }
        }

        fn judge(reply: &str) -> (Arc<Judge>, Faux) {
            let faux = Faux::new([FauxReply::text(reply)]);
            let w = crate::memory::Worker {
                adapter: Arc::new(faux.clone()),
                provider: "faux".into(),
                model: "small".into(),
            };
            (Arc::new(Judge::new(w, false)), faux)
        }

        fn policy(v: Value) -> Policy {
            Policy::new(serde_json::from_value(v).unwrap(), "/h".into(), None)
        }

        async fn auth(p: &Policy, cards: Option<&Arc<Cards>>, j: &Judge, cmd: &str) -> Option<ToolOutput> {
            let approver = cards.map(|c| c.clone() as Arc<dyn Approver>);
            let never = tokio_util::sync::CancellationToken::new();
            authorize_cancellable(
                p,
                approver.as_ref(),
                Some(j),
                "bash",
                false,
                &json!({ "command": cmd }),
                Path::new("/w"),
                &HookDecision::None,
                &never,
            )
            .await
        }

        fn text(o: &ToolOutput) -> String {
            format!("{:?}", o.content)
        }

        #[tokio::test]
        async fn allow_runs_and_the_judge_sees_the_user() {
            let (j, faux) = judge("ALLOW");
            j.see(crate::judge::Seen {
                session: "s".into(),
                user: vec!["push my branch".into()],
                calls: vec![],
            });
            assert!(
                auth(&policy(json!({})), None, &j, "git push origin main")
                    .await
                    .is_none()
            );
            let sent = format!("{:?}", faux.requests.lock().unwrap()[0].messages);
            assert!(sent.contains("push my branch"));
            assert_eq!(faux.requests.lock().unwrap().len(), 1);
            // Builds never reach it.
            assert!(auth(&policy(json!({})), None, &j, "cargo test").await.is_none());
            assert_eq!(faux.requests.lock().unwrap().len(), 1);
        }

        #[tokio::test]
        async fn block_asks_interactive_and_refuses_unattended() {
            let (j, _) = judge("BLOCK: the user never asked to publish");
            let p = policy(json!({}));
            let out = auth(&p, None, &j, "npm publish").await.unwrap();
            assert!(out.is_error);
            assert!(text(&out).contains("Refused by the command judge: the user never asked to publish."));

            let (j, _) = judge("BLOCK: pushes to a remote");
            let yes = Arc::new(Cards(Approval::Session, Mutex::default()));
            assert!(auth(&p, Some(&yes), &j, "git push").await.is_none());
            let req = yes.1.lock().unwrap()[0].clone();
            assert!(req.risky);
            assert_eq!(req.reason, "the command judge: pushes to a remote");
            // The yes is remembered for this exact command: no second judge call.
            assert!(p.user_allowed("git push"));

            let (j, _) = judge("BLOCK: deletes a remote");
            let no = Arc::new(Cards(Approval::Deny(None), Mutex::default()));
            let out = auth(&p, Some(&no), &j, "git push other").await.unwrap();
            assert!(text(&out).contains("The user declined this call."));
        }

        #[tokio::test]
        async fn unclear_answers_and_long_commands_block() {
            let (j, _) = judge("Sure, looks fine to me");
            let (tx, mut rx) = tokio::sync::broadcast::channel(4);
            j.set_events(tx);
            let out = auth(&policy(json!({})), None, &j, "curl https://x").await.unwrap();
            assert!(text(&out).contains("Refused by the command judge: the judge gave no clear answer."));
            assert!(rx.try_recv().is_err(), "no fallback notice");
            // Too long to judge: blocked without a model call.
            let (j, faux) = judge("ALLOW");
            let long = format!("curl https://x/{}", "a".repeat(crate::judge::MAX_COMMAND));
            let out = auth(&policy(json!({})), None, &j, &long).await.unwrap();
            assert!(text(&out).contains("too long to judge"));
            assert!(faux.requests.lock().unwrap().is_empty());
        }

        #[tokio::test]
        async fn error_falls_back_to_the_gate_with_a_notice() {
            // A model that never answers times out, the gate's verdict stands, and its request is cancelled.
            struct Silent(tokio_util::sync::CancellationToken);
            impl novacode_ai::adapter::Adapter for Silent {
                fn stream(
                    &self,
                    _: novacode_ai::adapter::Request,
                    cancel: tokio_util::sync::CancellationToken,
                ) -> tokio::sync::mpsc::Receiver<novacode_types::StreamEvent> {
                    let (tx, rx) = tokio::sync::mpsc::channel(1);
                    let seen = self.0.clone();
                    tokio::spawn(async move {
                        cancel.cancelled().await;
                        seen.cancel();
                        drop(tx);
                    });
                    rx
                }
            }
            let stopped = tokio_util::sync::CancellationToken::new();
            let slow = crate::memory::Worker {
                adapter: Arc::new(Silent(stopped.clone())),
                provider: "faux".into(),
                model: "m".into(),
            };
            let j = Judge::new(slow, false).with_timeout(std::time::Duration::from_millis(20));
            let (tx, mut rx) = tokio::sync::broadcast::channel(4);
            j.set_events(tx);
            assert!(auth(&policy(json!({})), None, &j, "curl https://x").await.is_none());
            assert!(format!("{:?}", rx.try_recv().unwrap()).contains("no answer in"));
            tokio::time::timeout(std::time::Duration::from_secs(5), stopped.cancelled())
                .await
                .unwrap();
        }

        #[tokio::test]
        async fn allow_rules_and_ask_mode_skip_it() {
            let (j, faux) = judge("BLOCK: no");
            let p = policy(json!({ "allow": ["Bash(git push:*)"] }));
            assert!(auth(&p, None, &j, "git push origin main").await.is_none());
            let ask = policy(json!({ "mode": "ask" }));
            let yes = Arc::new(Cards(Approval::Once, Mutex::default()));
            assert!(auth(&ask, Some(&yes), &j, "git push origin main").await.is_none());
            assert!(yes.1.lock().unwrap()[0].reason.starts_with("ask mode"));
            // A refusal by the gate stays a refusal.
            assert!(auth(&policy(json!({})), None, &j, "rm -rf ~").await.is_some());
            assert!(faux.requests.lock().unwrap().is_empty());
        }

        #[tokio::test]
        async fn unsandboxed_calls_are_judged() {
            let (j, faux) = judge("ALLOW");
            let never = tokio_util::sync::CancellationToken::new();
            let args = json!({ "action": "start", "command": "ls", "unsandboxed": true });
            let p = policy(json!({}));
            let out = authorize_cancellable(
                &p,
                None,
                Some(&j),
                "process",
                false,
                &args,
                Path::new("/w"),
                &HookDecision::None,
                &never,
            )
            .await;
            assert!(out.is_none());
            assert_eq!(faux.requests.lock().unwrap().len(), 1);
        }
    }

    fn call(name: &str, args: Value) -> ToolCall {
        ToolCall {
            id: "c".into(),
            name: name.into(),
            arguments: args,
            arguments_error: None,
            replay: None,
        }
    }

    #[test]
    fn lookup_repair_coerce() {
        let tools = novacode_tools::builtins(false);
        let o = match prepare(&tools, &call("Grepp", json!({}))) {
            Prepared::Rejected(o) => o,
            _ => panic!("expected a rejection"),
        };
        assert!(format!("{:?}", o.content).contains("Did you mean `grep`?"));
        let mut bad = call("read", json!({}));
        bad.arguments_error = Some("{oops".into());
        assert!(matches!(prepare(&tools, &bad), Prepared::Rejected(_)));
        let (args, accept) = match prepare(
            &tools,
            &call("read", json!({"path": "a", "offset": "3", "accept_large_output": true})),
        ) {
            Prepared::Ready(_, args, accept) => (args, accept),
            _ => panic!("expected ready"),
        };
        assert_eq!(args, json!({"path": "a", "offset": 3}));
        assert!(accept);
        let read = tools.iter().find(|t| t.name() == "read").unwrap();
        assert!(schema(read.as_ref())["properties"][ACCEPT_LARGE].is_object());
        let edit = tools.iter().find(|t| t.name() == "edit").unwrap();
        assert!(schema(edit.as_ref())["properties"].get(ACCEPT_LARGE).is_none());
    }
}
