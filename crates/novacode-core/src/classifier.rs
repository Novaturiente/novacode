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

    /// Classifier models whose provider has a key.
    pub fn available(&self) -> Vec<&'static classify::Model> {
        let cf = std::env::var("CLOUDFLARE_ACCOUNT_ID").is_ok();
        classify::MODELS
            .iter()
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
            let Some(key) = self.key(m.provider) else {
                return ToolOutput::error(format!(
                    "no key for {}: run `novacode login {}` or set its env var",
                    m.provider, m.provider
                ));
            };
            let account = std::env::var("CLOUDFLARE_ACCOUNT_ID").ok();
            let _slot = self.slots.acquire().await;
            let out = classify::classify(&novacode_ai::http::client(), m, &key, account.as_deref(), &ctx).await;
            structured(serde_json::to_value(out).unwrap_or_default())
        })
    }
}
