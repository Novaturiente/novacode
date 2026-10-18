//! `tool_search` and `call` (L5.17, L4.8): deferred tools stay out of the request
//! (no tokens, no cache change). `tool_search` ranks them with BM25. Found tools
//! are then announced in-line on models with native tool changes, or reached
//! through `call` everywhere else. The agent does both; these tools only find and
//! describe.

use crate::{Tool, ToolCtx, ToolOutput, parse, schema_for};
use futures_util::future::BoxFuture;
use novacode_types::ToolDef;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};

pub struct ToolSearch {
    pub tools: Vec<ToolDef>,
}

#[derive(Deserialize, JsonSchema)]
struct Args {
    /// What you need to do, in a few words (e.g. "query github issues").
    query: String,
    /// Most tools to return (default 5).
    limit: Option<usize>,
}

fn words(s: &str) -> Vec<String> {
    s.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.len() > 1)
        .map(String::from)
        .collect()
}

/// BM25 (k1 1.2, b 0.75) over name (counted twice) and description.
pub fn rank<'a>(tools: &'a [ToolDef], query: &str) -> Vec<(&'a ToolDef, f64)> {
    let docs: Vec<Vec<String>> = tools
        .iter()
        .map(|t| {
            let mut d = words(&t.name);
            d.extend(words(&t.name));
            d.extend(words(&t.description));
            d
        })
        .collect();
    let n = docs.len() as f64;
    let avg = docs.iter().map(Vec::len).sum::<usize>() as f64 / n.max(1.0);
    let q = words(query);
    let mut out: Vec<(&ToolDef, f64)> = tools
        .iter()
        .zip(&docs)
        .map(|(t, d)| {
            let score = q
                .iter()
                .map(|w| {
                    let df = docs.iter().filter(|d| d.contains(w)).count() as f64;
                    let tf = d.iter().filter(|x| *x == w).count() as f64;
                    let idf = ((n - df + 0.5) / (df + 0.5) + 1.0).ln();
                    idf * tf * 2.2 / (tf + 1.2 * (0.25 + 0.75 * d.len() as f64 / avg.max(1.0)))
                })
                .sum::<f64>();
            (t, score)
        })
        .filter(|(_, s)| *s > 0.0)
        .collect();
    out.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.name.cmp(&b.0.name)));
    out
}

impl Tool for ToolSearch {
    fn name(&self) -> &str {
        "tool_search"
    }
    fn description(&self) -> &str {
        "Find more tools by what they do. Many tools are not listed up front; search here when none of your tools fits. Found tools become callable."
    }
    fn schema(&self) -> Value {
        schema_for::<Args>()
    }
    fn sequential(&self) -> bool {
        true
    }
    fn large_output(&self) -> bool {
        false
    }
    fn execute<'a>(&'a self, args: Value, ctx: &'a ToolCtx) -> BoxFuture<'a, ToolOutput> {
        Box::pin(async move {
            let a: Args = match parse(args) {
                Ok(a) => a,
                Err(e) => return e,
            };
            let hits: Vec<&ToolDef> = rank(&self.tools, &a.query)
                .into_iter()
                .take(a.limit.unwrap_or(5).clamp(1, 20))
                .map(|(t, _)| t)
                .collect();
            if hits.is_empty() {
                let names: Vec<&str> = self.tools.iter().map(|t| t.name.as_str()).collect();
                return ToolOutput::text(format!(
                    "No tool matches `{}`. Searchable tools: {}",
                    a.query,
                    names.join(", ")
                ));
            }
            let names: Vec<&str> = hits.iter().map(|t| t.name.as_str()).collect();
            let text = if ctx.native_tools {
                let list: Vec<String> = hits
                    .iter()
                    .map(|t| format!("- {}: {}", t.name, t.description))
                    .collect();
                format!(
                    "These tools are now available; call them directly:\n{}",
                    list.join("\n")
                )
            } else {
                let list: Vec<String> = hits
                    .iter()
                    .map(|t| format!("## {}\n{}\nArguments schema: {}", t.name, t.description, t.schema))
                    .collect();
                format!(
                    "Call these through the `call` tool: {{\"name\": \"<tool>\", \"arguments\": {{...}}}}.\n\n{}",
                    list.join("\n\n")
                )
            };
            ToolOutput {
                details: Some(json!({ "activate": names })),
                ..ToolOutput::text(text)
            }
        })
    }
}

/// Runs a deferred tool by name. The agent rewrites the call into the real one
/// before the pipeline, so permissions and hooks apply; `execute` is never reached.
pub struct Call;

#[derive(Deserialize, JsonSchema)]
pub struct CallArgs {
    /// A tool found with tool_search.
    pub name: String,
    /// Its arguments, as its schema describes.
    #[serde(default)]
    pub arguments: Value,
}

impl Tool for Call {
    fn name(&self) -> &str {
        "call"
    }
    fn description(&self) -> &str {
        "Call a tool found with tool_search: {\"name\": tool, \"arguments\": {...}}."
    }
    fn schema(&self) -> Value {
        schema_for::<CallArgs>()
    }
    fn execute<'a>(&'a self, _args: Value, _ctx: &'a ToolCtx) -> BoxFuture<'a, ToolOutput> {
        Box::pin(async { ToolOutput::error("`call` needs a tool name found with tool_search") })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{ctx, text};

    fn def(name: &str, d: &str) -> ToolDef {
        ToolDef {
            name: name.into(),
            description: d.into(),
            schema: json!({"type": "object"}),
        }
    }

    #[tokio::test]
    async fn ranks_and_describes() {
        let tools = vec![
            def("github__list_issues", "List issues in a GitHub repository"),
            def("web_fetch", "Fetch a web page as markdown"),
            def("slack__post", "Post a message to a Slack channel"),
        ];
        let r = rank(&tools, "github issues");
        assert_eq!(r[0].0.name, "github__list_issues");
        assert_eq!(r.len(), 1);
        let t = ToolSearch { tools };
        let d = tempfile::tempdir().unwrap();
        let mut c = ctx(d.path());
        let o = t.execute(json!({"query": "fetch page"}), &c).await;
        assert!(text(&o).contains("through the `call` tool") && text(&o).contains("Arguments schema"));
        assert_eq!(o.details.unwrap()["activate"], json!(["web_fetch"]));
        c.native_tools = true;
        let o = t.execute(json!({"query": "slack message"}), &c).await;
        assert!(text(&o).starts_with("These tools are now available"), "{}", text(&o));
        assert!(text(&t.execute(json!({"query": "zzz"}), &c).await).contains("Searchable tools"));
    }
}
