//! Prompt-cache care (L2.17): a miss detector that names the cause, and an
//! opt-in warmer that keeps the cached prefix alive while it pays off.

use novacode_ai::adapter::{CacheRetention, Request};
use novacode_types::{Message, Usage};
use std::time::Duration;

/// The last request that touched the cache (a real one, or a warm replay).
#[derive(Clone)]
pub struct Sent {
    pub req: Request,
    pub provider: String,
    pub usage: Usage,
    pub at: std::time::Instant,
}

/// A detected miss: how many prompt tokens weren't read from cache, and why.
#[derive(Debug, Clone, PartialEq)]
pub struct Miss {
    pub lost: u64,
    pub cause: String,
}

/// Compare a reply's cache read with what the previous request left cached.
/// `None` when the cache held, or the provider doesn't report caching.
pub fn miss(prev: &Sent, provider: &str, req: &Request, usage: &Usage, ttl: Option<Duration>) -> Option<Miss> {
    if prev.usage.cache_read + prev.usage.cache_write == 0 {
        return None;
    }
    // Everything the previous request sent was cached (the marker sits on its last message).
    // Capped by this request's own size: a reply with server-run tools (web search) sums
    // usage over several internal passes, so its total counts the prefix twice.
    // Per-request tail text (extensions' before_request) sits after the marker: never cached.
    let tail = prev
        .req
        .tail
        .iter()
        .map(|t| novacode_types::estimate::text(t))
        .sum::<u64>();
    let expected = prev.usage.prompt_total().min(usage.prompt_total()).saturating_sub(tail);
    let noise = (expected / 20).max(1024);
    if usage.cache_read + noise >= expected {
        return None;
    }
    let lost = expected - usage.cache_read;
    let idle = prev.at.elapsed();
    let cause = if (provider, req.model.as_str()) != (prev.provider.as_str(), prev.req.model.as_str()) {
        format!(
            "model changed ({}/{} → {provider}/{})",
            prev.provider, prev.req.model, req.model
        )
    } else if req.tools != prev.req.tools {
        "tool list changed".to_string()
    } else if let Some(i) = first_difference(&prev.req.messages, &req.messages) {
        format!("prompt changed at message {} ({})", i + 1, role(&req.messages, i))
    } else if ttl.is_some_and(|t| idle > t) {
        format!(
            "expired: {} since the last request, lifetime {}",
            human(idle),
            human(ttl.unwrap_or_default())
        )
    } else if req.thinking != prev.req.thinking {
        "thinking level changed".to_string()
    } else {
        format!(
            "evicted by the provider: prefix unchanged, {} since the last request",
            human(idle)
        )
    };
    Some(Miss { lost, cause })
}

/// The first message of `prev` that `cur` doesn't repeat exactly.
fn first_difference(prev: &[Message], cur: &[Message]) -> Option<usize> {
    prev.iter()
        .zip(cur)
        .position(|(a, b)| a != b)
        .or_else(|| (cur.len() < prev.len()).then_some(cur.len()))
}

fn role(m: &[Message], i: usize) -> &'static str {
    match m.get(i) {
        Some(Message::System { .. }) => "system",
        Some(Message::User { .. }) => "user",
        Some(Message::Assistant(_)) => "assistant",
        Some(Message::ToolResult { .. }) => "tool result",
        Some(Message::Custom { .. }) => "custom",
        None => "removed",
    }
}

pub fn human(d: Duration) -> String {
    let s = d.as_secs();
    if s < 120 {
        format!("{s}s")
    } else {
        format!("{}m", s / 60)
    }
}

/// When to replay: pi's `min(90% of TTL, TTL − 10s)`.
pub fn warm_delay(ttl: Duration) -> Option<Duration> {
    (ttl > Duration::from_secs(10)).then(|| (ttl.mul_f64(0.9)).min(ttl - Duration::from_secs(10)))
}

/// Chance the user sends another message before the cache expires. While the
/// agent works the next request is certain. Idle numbers come from the user's own
/// trace (L2.17): ~44% come back in the next window after 5 idle minutes, ~10% later.
pub fn return_chance(busy: bool, idle_warms: u32) -> f64 {
    match (busy, idle_warms) {
        (true, _) => 1.0,
        (false, 0) => 0.44,
        _ => 0.10,
    }
}

/// Minimum expected saving, in input-token units (≈ pi's $0.05 at $5/M input).
pub const WARM_THRESHOLD: f64 = 10_000.0;

/// Expected saving of one warm replay, in input-token units, using Anthropic's
/// price ratios (read 0.1, write 1.25 / 2.0 for the 1h TTL, output 5). Units, not
/// dollars, so one rule serves subscriptions and API keys.
pub fn warm_saving(tokens: u64, retention: CacheRetention, chance: f64) -> f64 {
    let write = match retention {
        CacheRetention::Short => 1.25,
        CacheRetention::Long => 2.0,
    };
    let t = tokens as f64;
    chance * (write - 0.1) * t - (0.1 * t + 5.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    fn req(model: &str, msgs: &[&str]) -> Request {
        Request {
            tail: Vec::new(),
            model: model.into(),
            messages: msgs.iter().map(|m| Message::user(*m)).collect(),
            tools: vec![],
            max_tokens: 100,
            cache: CacheRetention::Short,
            thinking: None,
            images: true,
            session: None,
        }
    }

    fn usage(read: u64, write: u64, input: u64) -> Usage {
        Usage {
            input,
            cache_read: read,
            cache_write: write,
            ..Default::default()
        }
    }

    fn sent(r: Request, ago: u64) -> Sent {
        Sent {
            req: r,
            provider: "claude".into(),
            usage: usage(40_000, 10_000, 10),
            at: Instant::now() - Duration::from_secs(ago),
        }
    }

    #[test]
    fn server_tool_usage_is_not_a_miss() {
        // Live numbers: a reply with a server-side search, then a fully cached follow-up.
        let mut prev = sent(req("opus", &["a"]), 3);
        prev.usage = usage(9_452, 18_796, 6);
        let next = req("opus", &["a", "b"]);
        assert_eq!(miss(&prev, "claude", &next, &usage(18_796, 112, 4), None), None);
    }

    #[test]
    fn misses_name_their_cause() {
        let ttl = Some(Duration::from_secs(300));
        let prev = sent(req("opus", &["a", "b"]), 30);
        let next = req("opus", &["a", "b", "c"]);
        assert_eq!(miss(&prev, "claude", &next, &usage(50_000, 100, 4), ttl), None, "held");
        assert_eq!(miss(&prev, "claude", &next, &usage(49_000, 100, 4), ttl), None, "noise");
        let m = |r: &Request, p: &Sent| miss(p, "claude", r, &usage(0, 50_000, 4), ttl).unwrap().cause;
        assert!(m(&req("sonnet", &["a", "b", "c"]), &prev).starts_with("model changed"));
        assert_eq!(
            m(&req("opus", &["a", "X", "c"]), &prev),
            "prompt changed at message 2 (user)"
        );
        assert_eq!(
            m(&next, &sent(req("opus", &["a", "b"]), 400)),
            "expired: 6m since the last request, lifetime 5m"
        );
        assert!(m(&next, &prev).starts_with("evicted"));
        let lost = miss(&prev, "claude", &next, &usage(20_000, 30_100, 4), ttl)
            .unwrap()
            .lost;
        assert_eq!(lost, 30_010);
        // A provider that reports no caching never "misses".
        let mut quiet = prev.clone();
        quiet.usage = usage(0, 0, 50_000);
        assert_eq!(miss(&quiet, "claude", &next, &usage(0, 0, 50_000), ttl), None);
    }

    #[test]
    fn warming_economics() {
        assert_eq!(warm_delay(Duration::from_secs(300)), Some(Duration::from_secs(270)));
        assert_eq!(warm_delay(Duration::from_secs(3600)), Some(Duration::from_secs(3240)));
        let short = CacheRetention::Short;
        // While working: always worth it above a few thousand tokens.
        assert!(warm_saving(20_000, short, return_chance(true, 5)) > WARM_THRESHOLD);
        // Idle, 50k context: the first refresh pays, the next doesn't.
        assert!(warm_saving(50_000, short, return_chance(false, 0)) > WARM_THRESHOLD);
        assert!(warm_saving(50_000, short, return_chance(false, 1)) < WARM_THRESHOLD);
        // A tiny prompt never pays.
        assert!(warm_saving(3_000, short, 1.0) < WARM_THRESHOLD);
    }
}
