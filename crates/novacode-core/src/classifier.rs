//! The `classify` tool (L2.26): runs a classifier model. Codemode-only by default; scripts
//! reach it through `models.classify()`, so it still goes through the tool pipeline.

use futures_util::future::BoxFuture;
use novacode_ai::classify::{self, Context};
use novacode_ai::connect::Providers;
use novacode_tools::{Exposure, Tool, ToolCtx, ToolOutput};
use serde_json::{Value, json};
use std::sync::Arc;

pub struct Classify {
    pub providers: Arc<Providers>,
    /// pi's limit: at most four classifier calls at once.
    slots: Arc<tokio::sync::Semaphore>,
}

impl Classify {
    pub fn new(providers: Arc<Providers>) -> Self {
        Self {
            providers,
            slots: Arc::new(tokio::sync::Semaphore::new(4)),
        }
    }

    fn key(&self, provider: &str) -> Option<String> {
        self.providers.api_key(provider).or_else(|| {
            classify::key_env(provider)
                .iter()
                .find_map(|v| std::env::var(v).ok().filter(|k| !k.trim().is_empty()))
        })
    }

    /// One classification with `m` (the tool and the goal judge use it).
    pub async fn run(&self, m: &classify::Model, ctx: &Context) -> Result<classify::Outcome, String> {
        self.providers.allow_id(m.provider, m.id).map_err(|e| e.to_string())?;
        let Some(key) = self.key(m.provider) else {
            return Err(format!(
                "no key for {}: run `novacode login {}` or set its env var",
                m.provider, m.provider
            ));
        };
        let account = std::env::var("CLOUDFLARE_ACCOUNT_ID").ok();
        let _slot = self.slots.acquire().await;
        Ok(classify::classify(&novacode_ai::http::client(), m, &key, account.as_deref(), ctx).await)
    }

    /// Classifier models whose provider has a key, minus `models.deny` (L2.28).
    pub fn available(&self) -> Vec<&'static classify::Model> {
        let cf = std::env::var("CLOUDFLARE_ACCOUNT_ID").is_ok();
        classify::MODELS
            .iter()
            .filter(|m| self.providers.allow_id(m.provider, m.id).is_ok())
            .filter(|m| self.key(m.provider).is_some() && (m.wire != classify::Wire::Cloudflare || cf))
            .collect()
    }
}

fn structured(v: Value) -> ToolOutput {
    let mut out = ToolOutput::text(v.to_string());
    out.details = Some(json!({ "structured": v }));
    out
}

impl Tool for Classify {
    fn name(&self) -> &str {
        "classify"
    }
    fn description(&self) -> &str {
        "Answer typed questions (choice / score / bool, with probabilities) about JSON `state` with a classifier model. \
         `{list: true}` lists the classifier models you can use. Otherwise `{model: \"provider/id\", state, questions, images?}`."
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "list": { "type": "boolean", "description": "List available classifier models instead." },
                "model": { "type": "string", "description": "provider/id, e.g. openrouter/typesafe/jev-1.13" },
                "state": { "type": "object" },
                "images": { "type": "array", "items": { "type": "object" } },
                "questions": { "type": "object", "description": "id → {type: choice|score|bool, instructions, criteria}" }
            }
        })
    }
    fn exposure(&self) -> Exposure {
        Exposure::CodemodeOnly
    }
    fn large_output(&self) -> bool {
        false
    }
    fn execute<'a>(&'a self, args: Value, _ctx: &'a ToolCtx) -> BoxFuture<'a, ToolOutput> {
        Box::pin(async move {
            if args["list"] == true {
                return structured(json!(self.available()));
            }
            let Some(spec) = args["model"].as_str() else {
                return ToolOutput::error("`model` is required (see {list: true})");
            };
            let Some(m) = classify::find(spec) else {
                let known: Vec<String> = classify::MODELS
                    .iter()
                    .map(|m| format!("{}/{}", m.provider, m.id))
                    .collect();
                return ToolOutput::error(format!("unknown classifier `{spec}`; known: {}", known.join(", ")));
            };
            let ctx: Context = match serde_json::from_value(args.clone()) {
                Ok(c) => c,
                Err(e) => return ToolOutput::error(format!("bad classify arguments: {e}")),
            };
            match self.run(m, &ctx).await {
                Ok(out) => structured(serde_json::to_value(out).unwrap_or_default()),
                Err(e) => ToolOutput::error(e),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn denied_classifiers_are_refused_and_not_listed() {
        let mut p = Providers::load(std::path::Path::new("/nonexistent"), &Default::default());
        p.deny = vec!["*".into()];
        let c = Classify::new(Arc::new(p));
        assert!(c.available().is_empty());
        let m = classify::find("openrouter/typesafe/jev-1.13").unwrap();
        let ctx: Context = serde_json::from_value(json!({"state": {}, "questions": {}})).unwrap();
        let err = c.run(m, &ctx).await.unwrap_err();
        assert!(
            err.contains("openrouter/typesafe/jev-1.13") && err.contains("models.deny"),
            "{err}"
        );
    }
}
