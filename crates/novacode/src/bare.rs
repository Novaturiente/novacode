//! `--bare -p` (L8.17): one model call for editor completion. No tools, no system
//! prompt, no session, memory, skills, MCP, hooks or reminders: the adapter is called
//! directly, without building an `Agent`.

use crate::models;
use anyhow::bail;
use novacode_ai::adapter::{CacheRetention, Request};
use novacode_ai::connect::Providers;
use novacode_ai::trace::Trace;
use novacode_base::settings::Settings;
use novacode_types::{Block, Message, StreamEvent};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

pub struct Opts {
    pub max_tokens: Option<u32>,
    pub stop: Vec<String>,
}

/// Send `prompt` to `model` and return the answer text, cut at the first stop text.
pub async fn run(
    providers: &Arc<Providers>,
    settings: &Settings,
    model: &str,
    prompt: String,
    trace: Option<Trace>,
    opts: Opts,
) -> anyhow::Result<String> {
    let route = models::usable(providers, model)?;
    let (adapter, _) = providers.adapter(&route.provider, trace).await?;
    let (_, limit) = models::Limits::new(settings).of(&route);
    let req = Request {
        model: route.model.clone(),
        messages: vec![Message::User {
            content: vec![Block::text(prompt)],
        }],
        tools: vec![],
        max_tokens: opts.max_tokens.unwrap_or(limit),
        cache: CacheRetention::Short,
        // Only `model:level` sets thinking; otherwise none is sent (fastest).
        thinking: route.level.map(|l| route.clamp(l)),
        images: false,
        session: None,
        tail: vec![],
        stop: opts.stop.clone(),
    };
    let mut rx = adapter.stream(req, CancellationToken::new());
    while let Some(ev) = rx.recv().await {
        match ev {
            StreamEvent::Done(m) => return Ok(cut(&m.text(), &opts.stop).to_string()),
            StreamEvent::Error(m) => bail!(m.error_message.unwrap_or_else(|| format!("{:?}", m.stop))),
            _ => {}
        }
    }
    bail!("stream ended without a result")
}

/// The text before the earliest stop text. Providers without stop sequences
/// (OpenAI Responses) run on, so this keeps the output the same everywhere.
fn cut<'a>(text: &'a str, stop: &[String]) -> &'a str {
    let end = stop
        .iter()
        .filter(|s| !s.is_empty())
        .filter_map(|s| text.find(s.as_str()))
        .min()
        .unwrap_or(text.len());
    &text[..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cuts_at_the_earliest_stop() {
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        assert_eq!(cut("a</c>b<x>", &s(&["<x>", "</c>"])), "a");
        assert_eq!(cut("abc", &s(&["zz", ""])), "abc");
        assert_eq!(cut("abc", &[]), "abc");
    }
}
