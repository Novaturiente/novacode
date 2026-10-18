//! `/usage` (L11.4): tokens and cost per route for this session, warm and memory
//! usage apart, plus the quota windows of every subscription you're logged in to.

use std::collections::BTreeMap;
use std::fmt::Write;
use std::path::Path;

use novacode_ai::auth::AuthStore;
use novacode_ai::connect::Providers;
use novacode_core::session::{Entry, EntryKind};
use novacode_types::{Message, Usage};

#[derive(Default)]
struct Row {
    calls: u64,
    usage: Usage,
    usd: f64,
}

fn add(rows: &mut BTreeMap<String, Row>, key: String, u: &Usage, usd: f64) {
    let r = rows.entry(key).or_default();
    r.calls += 1;
    r.usage.input += u.input;
    r.usage.output += u.output;
    r.usage.cache_read += u.cache_read;
    r.usage.cache_write += u.cache_write;
    r.usd += usd;
}

/// The session part: one row per route, then warm replays and memory workers.
pub fn report(p: &Providers, entries: &[Entry]) -> String {
    let mut route = String::from("?");
    let mut price = None;
    let (mut chat, mut warm, mut memory) = (BTreeMap::new(), BTreeMap::new(), BTreeMap::new());
    for e in entries {
        match &e.kind {
            EntryKind::ModelChange { provider, model } => {
                route = format!("{provider}/{model}");
                price = p.resolve(&route).ok().and_then(|r| crate::models::price(p, &r));
            }
            EntryKind::Message {
                message: Message::Assistant(a),
            } => {
                let usd = price.as_ref().map_or(0.0, |c| a.usage.cost(c));
                add(&mut chat, route.clone(), &a.usage, usd);
            }
            EntryKind::WarmUsage { provider, model, usage } => {
                let key = format!("{provider}/{model}");
                let usd = p
                    .resolve(&key)
                    .ok()
                    .and_then(|r| crate::models::price(p, &r))
                    .map_or(0.0, |c| usage.cost(&c));
                add(&mut warm, key, usage, usd);
            }
            EntryKind::Notes { usage, model, .. } => add(&mut memory, format!("observer {model}"), usage, 0.0),
            EntryKind::Reflections { usage, model, .. } => add(&mut memory, format!("reflector {model}"), usage, 0.0),
            EntryKind::Drops { usage, model, .. } => add(&mut memory, format!("dropper {model}"), usage, 0.0),
            _ => {}
        }
    }
    let mut s = String::new();
    let _ = writeln!(
        s,
        "  {:<34} {:>5} {:>9} {:>9} {:>9} {:>8} {:>6} {:>8}",
        "", "calls", "input", "cache rd", "cache wr", "output", "hit", "cost"
    );
    let mut total = Row::default();
    for (title, rows) in [
        ("This session", &chat),
        ("Cache warming", &warm),
        ("Memory workers", &memory),
    ] {
        if rows.is_empty() {
            continue;
        }
        let _ = writeln!(s, "{title}");
        for (k, r) in rows {
            let _ = writeln!(s, "  {}", line(k, r));
            total.calls += r.calls;
            total.usd += r.usd;
            for (a, b) in [
                (&mut total.usage.input, r.usage.input),
                (&mut total.usage.output, r.usage.output),
                (&mut total.usage.cache_read, r.usage.cache_read),
                (&mut total.usage.cache_write, r.usage.cache_write),
            ] {
                *a += b;
            }
        }
    }
    if total.calls == 0 {
        return "No model calls in this session yet.\n".into();
    }
    let _ = writeln!(s, "  {}", line("total", &total));
    s.push_str("  cost is shown for API-key routes only; subscriptions count against the windows below\n");
    s
}

fn line(k: &str, r: &Row) -> String {
    let u = &r.usage;
    let hit = u.cache_read as f64 * 100.0 / u.prompt_total().max(1) as f64;
    let k: String = k.chars().take(34).collect();
    format!(
        "{k:<34} {:>5} {:>9} {:>9} {:>9} {:>8} {:>5.0}% {:>8}",
        r.calls,
        num(u.input),
        num(u.cache_read),
        num(u.cache_write),
        num(u.output),
        hit,
        if r.usd > 0.0 {
            format!("${:.4}", r.usd)
        } else {
            "-".into()
        }
    )
}

fn num(n: u64) -> String {
    match n {
        0..1_000 => n.to_string(),
        1_000..1_000_000 => format!("{:.1}k", n as f64 / 1e3),
        _ => format!("{:.2}M", n as f64 / 1e6),
    }
}

/// Quota windows of every subscription with a login, read through the shared cache.
pub async fn quotas(home: &Path, p: &Providers, current: &str) -> String {
    let auth = AuthStore::new(home.join("auth.json"));
    let (cur_provider, cur_model) = current.split_once('/').unwrap_or((current, ""));
    let mut s = String::from("Subscription windows\n");
    let mut any = false;
    for provider in p
        .profiles
        .values()
        .filter(|pr| novacode_ai::quota::supported(&pr.id) && p.connected(pr))
        .map(|pr| pr.id.clone())
    {
        let model = if provider == cur_provider { cur_model } else { "" };
        let Some(q) = novacode_ai::quota::cached(home, &auth, &provider, model).await else {
            continue;
        };
        any = true;
        let now = novacode_core::session::now_ms();
        let cols: Vec<String> = [("5h", &q.five_hour), ("week", &q.weekly), ("month", &q.monthly)]
            .into_iter()
            .filter_map(|(name, w)| {
                let w = w.as_ref()?;
                let reset = w.resets_in(now).map(|r| format!(", resets {r}")).unwrap_or_default();
                Some(format!("{name} {:>3.0}%{reset:<18}", w.used_pct))
            })
            .collect();
        let cols = if cols.is_empty() {
            "no windows reported".to_string()
        } else {
            cols.join("  ")
        };
        let _ = writeln!(s, "  {provider:<12} {cols}");
        if let Some(e) = &q.error {
            let _ = writeln!(s, "  {:<12} last fetch failed: {e}", "");
        }
    }
    if !any {
        s.push_str("  none (log in to Claude, ChatGPT/Codex or Antigravity to see them)\n");
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use novacode_types::{AssistantMessage, StopReason};

    fn entry(kind: EntryKind) -> Entry {
        Entry {
            id: "x".into(),
            parent: None,
            ts_ms: 0,
            kind,
        }
    }

    #[test]
    fn groups_by_route_with_warm_and_memory_apart() {
        let p = Providers::load(Path::new("/nonexistent"), &novacode_base::settings::Settings::default());
        let u = |i, r| Usage {
            input: i,
            output: 10,
            cache_read: r,
            cache_write: 0,
            reasoning: None,
        };
        let reply = |usage| {
            entry(EntryKind::Message {
                message: Message::Assistant(AssistantMessage {
                    content: vec![],
                    stop: StopReason::Stop,
                    usage,
                    provider: "anthropic".into(),
                    model: "m".into(),
                    response_id: None,
                    error_message: None,
                    thinking_level: None,
                }),
            })
        };
        let entries = vec![
            entry(EntryKind::ModelChange {
                provider: "claude".into(),
                model: "claude-opus-5-5".into(),
            }),
            reply(u(100, 900)),
            reply(u(100, 900)),
            entry(EntryKind::WarmUsage {
                provider: "claude".into(),
                model: "claude-opus-5-5".into(),
                usage: u(1, 1000),
            }),
            entry(EntryKind::Notes {
                notes: vec![],
                covers: "x".into(),
                usage: u(500, 0),
                model: "claude-haiku-5-5".into(),
            }),
        ];
        let r = report(&p, &entries);
        assert!(
            r.contains("This session") && r.contains("Cache warming") && r.contains("Memory workers"),
            "{r}"
        );
        let opus = r.lines().find(|l| l.contains("claude/claude-opus-5-5")).unwrap();
        assert!(
            opus.contains("    2 ") && opus.contains("1.8k") && opus.contains("90%"),
            "{opus}"
        );
        assert!(r.lines().any(|l| l.contains("observer claude-haiku-5-5")));
        assert!(
            r.lines()
                .any(|l| l.trim_start().starts_with("total") && l.contains("    4 ")),
            "{r}"
        );
        assert_eq!(report(&p, &[]), "No model calls in this session yet.\n");
    }
}
