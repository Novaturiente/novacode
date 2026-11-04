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
        // `maxTokens` is the answer's room. Every provider counts thinking against the
        // cap it is sent, so a reasoning model gets 16k more for thinking on top.
        let thinking = if r.info.reasoning { 16_384 } else { 0 };
        let max = (self.max_tokens.unwrap_or(16_384).saturating_add(thinking)).min(r.info.output.max(1024) as u32);
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

/// Resolve `spec` and refuse it when `models.deny` blocks it (L2.28).
pub fn usable(p: &Providers, spec: &str) -> Result<Route, novacode_ai::connect::ConnectError> {
    let r = p.resolve(spec)?;
    p.allow(&r)?;
    Ok(r)
}

/// The session's route at start: a resumed session's own model, else `model`. A resumed
/// model that no longer resolves or is denied falls back to `model`, with a notice.
pub fn session_route(p: &Providers, resumed: Option<&str>, model: &str) -> anyhow::Result<(Route, Option<String>)> {
    let note = match resumed.map(|r| (r, usable(p, r))) {
        Some((_, Ok(r))) => return Ok((r, None)),
        Some((r, Err(e))) => Some(format!("this session's model `{r}` can't be used ({e})")),
        None => None,
    };
    let route = usable(p, model)?;
    let note = note.map(|n| format!("{n}; using {}", route.spec()));
    Ok((route, note))
}

/// The subagent model check (L2.28): a `provider/model[:level]` that `models.deny` blocks
/// is refused at spawn. Other resolve errors are left to the child's own start.
pub fn spawn_check(p: Arc<Providers>) -> crate::agents::ModelCheck {
    Arc::new(move |spec: &str| match p.resolve(spec) {
        Ok(r) => p.allow(&r).map_err(|e| e.to_string()),
        Err(_) => Ok(()),
    })
}

/// Everything the agent needs to move to `spec` (another provider is fine).
pub async fn switch(
    p: &Arc<Providers>,
    spec: &str,
    level: ThinkingLevel,
    limits: Limits,
    trace: Option<Trace>,
) -> anyhow::Result<(Switch, Route)> {
    let route = usable(p, spec)?;
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
/// One that can't resolve, is denied (`models.deny`) or can't connect falls back to the
/// session's; the second value is then a notice for the user.
pub async fn helper(
    p: &Arc<Providers>,
    explicit: Option<String>,
    route: &Route,
    main: Arc<dyn Adapter>,
) -> (novacode_core::memory::Worker, Option<String>) {
    let small = p.profile(&route.provider).ok().and_then(|pr| {
        pr.extra
            .get("smallModel")?
            .as_str()
            .map(|m| format!("{}/{m}", route.provider))
    });
    let session = novacode_core::memory::Worker {
        adapter: main.clone(),
        provider: route.provider.clone(),
        model: route.model.clone(),
    };
    let Some(spec) = explicit.or(small) else {
        return (session, None);
    };
    let fallback = |e: &dyn std::fmt::Display| {
        let note = format!("helper model `{spec}`: {e}; using {}", route.spec());
        tracing::warn!("{note}");
        (session, Some(note))
    };
    let r = match usable(p, &spec) {
        Ok(r) => r,
        Err(e) => return fallback(&e),
    };
    let adapter = if r.provider == route.provider {
        main.clone()
    } else {
        match p.adapter(&r.provider, None).await {
            Ok((a, _)) => a,
            Err(e) => return fallback(&e),
        }
    };
    let w = novacode_core::memory::Worker {
        adapter,
        provider: r.provider,
        model: r.model,
    };
    (w, None)
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
        // `models.deny` (L2.28): denied models aren't offered.
        for id in ids.into_iter().filter(|id| p.allow_id(&pr.id, id).is_ok()) {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn denying(globs: &[&str]) -> Arc<Providers> {
        let mut p = Providers::load(std::path::Path::new("/nonexistent"), &Settings::default());
        p.deny = globs.iter().map(|g| g.to_string()).collect();
        Arc::new(p)
    }

    #[tokio::test]
    async fn switch_refuses_a_denied_model() {
        let p = denying(&["claude-fable*"]);
        let limits = Limits::new(&Settings::default());
        let err = switch(&p, "claude/claude-fable-5", ThinkingLevel::Medium, limits, None)
            .await
            .err()
            .unwrap()
            .to_string();
        assert!(
            err.contains("claude/claude-fable-5") && err.contains("models.deny"),
            "{err}"
        );
    }

    #[test]
    fn start_refuses_and_resume_falls_back_with_a_notice() {
        let p = denying(&["claude-fable*"]);
        let err = session_route(&p, None, "claude/claude-fable-5")
            .err()
            .unwrap()
            .to_string();
        assert!(err.contains("models.deny"), "{err}");
        let (r, note) = session_route(&p, Some("claude/claude-fable-5"), "claude/claude-opus-4-5").unwrap();
        assert_eq!(r.spec(), "claude/claude-opus-4-5");
        let note = note.expect("the fallback is shown");
        assert!(
            note.contains("claude/claude-fable-5") && note.contains("models.deny") && note.contains("claude-opus-4-5"),
            "{note}"
        );
        let (r, note) = session_route(&p, Some("claude/claude-opus-4-5"), "claude/claude-sonnet-4-5").unwrap();
        assert_eq!((r.spec().as_str(), note), ("claude/claude-opus-4-5", None));
        // A resumed model that no longer resolves falls back visibly too.
        let (_, note) = session_route(&p, Some("gone/x"), "claude/claude-opus-4-5").unwrap();
        assert!(note.is_some_and(|n| n.contains("gone/x")));
    }

    #[test]
    fn subagent_models_are_checked_after_alias_resolution() {
        let p = denying(&["claude-opus-*"]);
        let check = spawn_check(p);
        let err = check("claude/claude-opus:high").unwrap_err();
        assert!(err.contains("models.deny"), "{err}");
        assert!(check("claude/claude-haiku-4-5").is_ok());
        assert!(
            check("nope/x").is_ok(),
            "an unknown provider fails at the child's start"
        );
    }

    #[tokio::test]
    async fn a_denied_helper_falls_back_with_a_notice() {
        let p = denying(&["claude-haiku*"]);
        let route = p.resolve("claude/claude-opus-4-5").unwrap();
        let main: Arc<dyn Adapter> = Arc::new(novacode_ai::faux::Faux::new([]));
        let (w, note) = helper(&p, Some("claude/claude-haiku-4-5".into()), &route, main).await;
        assert_eq!((w.provider.as_str(), w.model.as_str()), ("claude", "claude-opus-4-5"));
        let note = note.expect("the fallback is shown");
        assert!(
            note.contains("models.deny") && note.contains("claude-opus-4-5"),
            "{note}"
        );
        // The provider's small model (`claude-haiku`, an alias) is denied too.
        let (w, note) = helper(&p, None, &route, Arc::new(novacode_ai::faux::Faux::new([]))).await;
        assert_eq!(w.model, "claude-opus-4-5");
        assert!(note.is_some_and(|n| n.contains("models.deny")));
    }
}
