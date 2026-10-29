//! Model choice across providers (L2.10, L2.11): limits, switching, the
//! picker list and `novacode models`.

use futures_util::future::join_all;
use novacode_ai::adapter::Adapter;
use novacode_ai::connect::{Providers, Route};
use novacode_ai::trace::Trace;
use novacode_base::settings::Settings;
use novacode_core::agent::Switch;
use novacode_tui::dialog::PickItem;
use novacode_types::ThinkingLevel;
use std::process::ExitCode;
use std::sync::Arc;

/// Settings that cap what the catalog says.
#[derive(Clone, Copy)]
pub struct Limits {
    max_tokens: Option<u32>,
    window: Option<u64>,
}

impl Limits {
    pub fn new(s: &Settings) -> Self {
        Self {
            max_tokens: s.get("maxTokens"),
            window: s.get("contextWindow"),
        }
    }

    /// (context window, max output tokens) for a route.
    pub fn of(&self, r: &Route) -> (u64, u32) {
        let window = self.window.unwrap_or(r.info.context);
        // Every provider counts thinking against this cap: a reasoning model gets 16k
        // more by default, so its thinking can't eat the answer's 16k.
        let default = if r.info.reasoning { 32_768 } else { 16_384 };
        let max = self.max_tokens.unwrap_or(default).min(r.info.output.max(1024) as u32);
        (window, max)
    }
}

/// API prices of a route, or `None` on a subscription: subscription spend isn't
/// something novacode can compute, so no cost is shown there (L2.21).
pub fn price(p: &Providers, route: &Route) -> Option<novacode_types::Cost> {
    p.profile(&route.provider)
        .ok()
        .filter(|pr| !pr.oauth())
        .and(route.info.cost)
}

/// USD spent on API-key routes across a session's replies, each at the price of the
/// route recorded before it. A reply's own `provider` is its replay namespace, which
/// the Claude login shares with the Anthropic API key, so it can't tell them apart.
pub fn spent(p: &Providers, entries: &[novacode_core::session::Entry]) -> f64 {
    use novacode_core::session::EntryKind;
    let mut price_now = None;
    let mut sum = 0.0;
    for e in entries {
        match &e.kind {
            EntryKind::ModelChange { provider, model } => {
                price_now = p
                    .resolve(&format!("{provider}/{model}"))
                    .ok()
                    .and_then(|r| price(p, &r));
            }
            EntryKind::Message {
                message: novacode_types::Message::Assistant(a),
            } => sum += price_now.as_ref().map_or(0.0, |c| a.usage.cost(c)),
            _ => {}
        }
    }
    sum
}

/// Everything the agent needs to move to `spec` (another provider is fine).
pub async fn switch(
    p: &Arc<Providers>,
    spec: &str,
    level: ThinkingLevel,
    limits: Limits,
    trace: Option<Trace>,
) -> anyhow::Result<(Switch, Route)> {
    let route = p.resolve(spec)?;
    let (adapter, _) = p.adapter(&route.provider, trace).await?;
    let (window_tokens, max_tokens) = limits.of(&route);
    let switch = Switch {
        provider: route.provider.clone(),
        model: route.model.clone(),
        adapter,
        window_tokens,
        max_tokens,
        images: route.info.images,
        reasoning: route.info.reasoning,
        thinking: route.clamp(route.level.unwrap_or(level)),
        helper: None,
    };
    Ok((switch, route))
}

/// The background model (observer, goal judge) for a session on `route`: `explicit`
/// (`memory.model`), else the provider's small model, else the session's own.
/// One that can't connect falls back to the session's.
pub async fn helper(
    p: &Arc<Providers>,
    explicit: Option<String>,
    route: &Route,
    main: Arc<dyn Adapter>,
) -> novacode_core::memory::Worker {
    let small = p.profile(&route.provider).ok().and_then(|pr| {
        pr.extra
            .get("smallModel")?
            .as_str()
            .map(|m| format!("{}/{m}", route.provider))
    });
    let fallback = || novacode_core::memory::Worker {
        adapter: main.clone(),
        provider: route.provider.clone(),
        model: route.model.clone(),
    };
    let Some(spec) = explicit.or(small) else {
        return fallback();
    };
    let r = match p.resolve(&spec) {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(%e, "memory.model");
            return fallback();
        }
    };
    let adapter = if r.provider == route.provider {
        main.clone()
    } else {
        match p.adapter(&r.provider, None).await {
            Ok((a, _)) => a,
            Err(e) => {
                tracing::warn!(%e, "memory.model connect");
                return fallback();
            }
        }
    };
    novacode_core::memory::Worker {
        adapter,
        provider: r.provider,
        model: r.model,
    }
}

/// Live model lists of every connected provider, plus errors worth showing.
pub async fn list(p: &Arc<Providers>, current: &str) -> (Vec<PickItem>, Vec<String>) {
    let connected: Vec<_> = p.profiles.values().filter(|pr| p.connected(pr)).collect();
    let lists = join_all(connected.iter().map(|pr| async move {
        let ids = match p.adapter(&pr.id, None).await {
            Ok((a, _)) => a.list_models().await,
            Err(e) => Err(e.to_string()),
        };
        (*pr, ids)
    }))
    .await;
    let mut items = Vec::new();
    let mut errors = Vec::new();
    for (pr, ids) in lists {
        let ids = match ids {
            Ok(ids) => ids,
            // Local servers that aren't running are not an error worth a notice.
            Err(_) if pr.key_optional => continue,
            Err(e) => {
                errors.push(format!("{}: {e}", pr.id));
                continue;
            }
        };
        for id in ids {
            let value = format!("{}/{id}", pr.id);
            let info = p.catalog.get(pr.catalog_id(), &id);
            let mut detail = info.map(|i| format!("{}k", i.context / 1000)).unwrap_or_default();
            if value == current {
                detail = format!("current · {detail}");
            }
            items.push(PickItem {
                label: value.clone(),
                detail,
                value,
            });
        }
    }
    // The current provider first, then by name.
    let cur = current.split_once('/').map_or("", |(p, _)| p);
    items.sort_by_key(|i| (!i.value.starts_with(&format!("{cur}/")), i.value.clone()));
    (items, errors)
}

/// `novacode models [filter]`: connected providers' models with their metadata.
pub async fn command(p: &Arc<Providers>, filter: Option<String>, all: bool) -> anyhow::Result<ExitCode> {
    let f = filter.unwrap_or_default().to_lowercase();
    if all {
        // Every provider novacode knows, connected or not.
        for pr in p.profiles.values() {
            let state = if p.connected(pr) { "connected" } else { "" };
            let env = pr
                .env
                .first()
                .map(String::as_str)
                .unwrap_or(if pr.oauth() { "login" } else { "" });
            if pr.id.contains(&f) || pr.name.to_lowercase().contains(&f) {
                println!("{:24} {:32} {:28} {state}", pr.id, pr.name, env);
            }
        }
        return Ok(ExitCode::SUCCESS);
    }
    let (items, errors) = list(p, "").await;
    for e in &errors {
        eprintln!("warning: {e}");
    }
    let mut shown = 0;
    for it in items.iter().filter(|i| i.value.to_lowercase().contains(&f)) {
        let (prov, id) = it.value.split_once('/').unwrap_or_default();
        let info = p.profiles.get(prov).map(|pr| p.catalog.info(pr.catalog_id(), id));
        let Some(i) = info else { continue };
        let cost = i
            .cost
            .map(|c| format!("${}/{} per M", c.input, c.output))
            .unwrap_or_default();
        let think = if i.reasoning { "thinking" } else { "" };
        let img = if i.images { "images" } else { "" };
        let ctx = if i.known {
            format!("{}k", i.context / 1000)
        } else {
            "?".into()
        };
        println!("{:52} {:>6}  {:8} {:6} {cost}", it.value, ctx, think, img);
        shown += 1;
    }
    if shown == 0 {
        eprintln!(
            "no models{}; `novacode models --all` lists providers and how to connect them",
            if f.is_empty() {
                String::new()
            } else {
                format!(" match `{f}`")
            }
        );
    }
    Ok(ExitCode::SUCCESS)
}
