//! The tool pipeline (L3.2): lookup with a name suggestion, argument repair
//! and lenient coercion, permission check (the agent's preflight, L6.1),
//! execute, guard and redact the output once. Every call path uses it.
// shortcut: no before_tool/after_tool hooks yet; they slot in next to the permission check (M10).

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
        let Prepared::Rejected(o) = prepare(&tools, &call("Grepp", json!({}))) else {
            panic!()
        };
        assert!(format!("{:?}", o.content).contains("Did you mean `grep`?"));
        let mut bad = call("read", json!({}));
        bad.arguments_error = Some("{oops".into());
        assert!(matches!(prepare(&tools, &bad), Prepared::Rejected(_)));
        let Prepared::Ready(_, args, accept) = prepare(
            &tools,
            &call("read", json!({"path": "a", "offset": "3", "accept_large_output": true})),
        ) else {
            panic!()
        };
        assert_eq!(args, json!({"path": "a", "offset": 3}));
        assert!(accept);
        let read = tools.iter().find(|t| t.name() == "read").unwrap();
        assert!(schema(read.as_ref())["properties"][ACCEPT_LARGE].is_object());
        let edit = tools.iter().find(|t| t.name() == "edit").unwrap();
        assert!(schema(edit.as_ref())["properties"].get(ACCEPT_LARGE).is_none());
    }
}
