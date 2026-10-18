//! The adapter contract (L2.1) and the shared message builder (L2.3).

use crate::json::parse_tool_args;
use novacode_types::{
    AssistantMessage, Block, BlockKind, ErrorKind, Message, Replay, StopReason, StreamEvent, ThinkingLevel, ToolCall,
    ToolDef, Usage,
};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// Cache retention requested from the provider (L2.17).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum CacheRetention {
    /// Provider default (Anthropic: 5 minutes).
    #[default]
    Short,
    /// Long TTL where offered (Anthropic: 1 hour).
    Long,
}

/// Everything an adapter needs for one model call.
#[derive(Debug, Clone)]
pub struct Request {
    pub model: String,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolDef>,
    pub max_tokens: u32,
    pub cache: CacheRetention,
    /// `None` = send no thinking setting (the model's default).
    pub thinking: Option<ThinkingLevel>,
    /// The model takes images; otherwise they become a placeholder (L2.15).
    pub images: bool,
    /// Session id, for providers that key their cache on it (L2.17).
    pub session: Option<String>,
}

/// One wire protocol. Stateless per call: `stream` never fails, it reports
/// failure as a terminal [`StreamEvent::Error`] (L2.1, "the loop never throws").
pub trait Adapter: Send + Sync {
    fn stream(&self, req: Request, cancel: CancellationToken) -> mpsc::Receiver<StreamEvent>;
    /// How long the provider keeps this request's prefix cached, when replaying it
    /// with `max_tokens: 1` is a cheap way to keep it alive (L2.17 warmer). `None` = don't warm.
    fn warm_ttl(&self, _req: &Request) -> Option<std::time::Duration> {
        None
    }
    /// Model ids this account can use right now (the live list, L2.10).
    fn list_models(&self) -> futures_util::future::BoxFuture<'_, Result<Vec<String>, String>> {
        Box::pin(async { Err("this provider has no model list".to_string()) })
    }
}

/// Bound on queued events: a slow consumer back-pressures the adapter (principle 5).
pub const EVENT_BUFFER: usize = 256;

/// A block being streamed.
#[derive(Debug)]
struct Partial {
    kind: BlockKind,
    text: String,
    id: String,
    name: String,
    replay: Option<Replay>,
    native: Option<serde_json::Value>,
}

/// Turns adapter-level block start/delta/end calls into [`StreamEvent`]s and
/// the final [`AssistantMessage`]. Every adapter uses this one builder.
pub struct Builder {
    tx: mpsc::Sender<StreamEvent>,
    provider: String,
    model: String,
    open: Vec<(usize, Partial)>,
    done: Vec<(usize, Block)>,
    pub usage: Usage,
    pub response_id: Option<String>,
    /// Set once any content was streamed; providers retry only before this (L2.1).
    pub started: bool,
    /// Stamped on the final message (see [`AssistantMessage::thinking_level`]).
    pub thinking_level: Option<String>,
}

impl Builder {
    pub fn new(tx: mpsc::Sender<StreamEvent>, provider: &str, model: &str) -> Self {
        Self {
            tx,
            provider: provider.into(),
            model: model.into(),
            open: Vec::new(),
            done: Vec::new(),
            usage: Usage::default(),
            response_id: None,
            started: false,
            thinking_level: None,
        }
    }

    pub fn provider(&self) -> &str {
        &self.provider
    }

    async fn send(&self, ev: StreamEvent) {
        let _ = self.tx.send(ev).await; // receiver gone = caller stopped listening
    }

    pub async fn start(&mut self, index: usize, kind: BlockKind, id: &str, name: &str) {
        self.started = true;
        let p = Partial {
            kind,
            text: String::new(),
            id: id.into(),
            name: name.into(),
            replay: None,
            native: None,
        };
        self.open.push((index, p));
        let name = (kind == BlockKind::ToolCall).then(|| name.to_string());
        self.send(StreamEvent::BlockStart { index, kind, name }).await;
    }

    pub fn start_native(&mut self, index: usize, item: serde_json::Value) {
        self.started = true;
        let mut p = Partial {
            kind: BlockKind::ProviderNative,
            text: String::new(),
            id: String::new(),
            name: String::new(),
            replay: None,
            native: None,
        };
        p.native = Some(item);
        self.open.push((index, p));
    }

    fn partial(&mut self, index: usize) -> Option<&mut Partial> {
        self.open.iter_mut().find(|(i, _)| *i == index).map(|(_, p)| p)
    }

    pub async fn delta(&mut self, index: usize, text: &str) {
        let Some(p) = self.partial(index) else { return };
        p.text.push_str(text);
        if p.kind != BlockKind::ProviderNative {
            self.send(StreamEvent::Delta {
                index,
                text: text.into(),
            })
            .await;
        }
    }

    /// Text streamed so far into an open block.
    pub fn partial_text(&self, index: usize) -> Option<&str> {
        self.open
            .iter()
            .find(|(i, _)| *i == index)
            .map(|(_, p)| p.text.as_str())
    }

    pub fn set_replay(&mut self, index: usize, data: serde_json::Value) {
        let provider = self.provider.clone();
        if let Some(p) = self.partial(index) {
            p.replay = Some(Replay { provider, data });
        }
    }

    pub async fn end(&mut self, index: usize) {
        let Some(pos) = self.open.iter().position(|(i, _)| *i == index) else {
            return;
        };
        let (_, p) = self.open.remove(pos);
        let block = match p.kind {
            BlockKind::Text => Block::Text {
                text: p.text,
                replay: p.replay,
            },
            BlockKind::Thinking => Block::Thinking {
                text: p.text,
                replay: p.replay,
            },
            BlockKind::ToolCall => {
                let (arguments, arguments_error) = parse_tool_args(&p.text);
                Block::ToolCall(ToolCall {
                    id: p.id,
                    name: p.name,
                    arguments,
                    arguments_error,
                    replay: p.replay,
                })
            }
            BlockKind::ProviderNative => {
                let mut item = p.native.unwrap_or_default();
                if !p.text.is_empty()
                    && let Some(obj) = item.as_object_mut()
                {
                    obj.insert("input".into(), parse_tool_args(&p.text).0);
                }
                Block::ProviderNative {
                    provider: self.provider.clone(),
                    item,
                }
            }
        };
        if !matches!(block, Block::ProviderNative { .. }) {
            self.send(StreamEvent::BlockEnd {
                index,
                block: block.clone(),
            })
            .await;
        }
        self.done.push((index, block));
    }

    pub async fn usage(&mut self, usage: Usage) {
        self.usage = usage;
        self.send(StreamEvent::Usage(usage)).await;
    }

    /// Drop everything so far (before a retry).
    pub async fn rollback(&mut self) {
        if self.started {
            self.send(StreamEvent::RetryRollback).await;
        }
        self.open.clear();
        self.done.clear();
        self.usage = Usage::default();
        self.started = false;
    }

    fn message(&mut self, stop: StopReason, error_message: Option<String>) -> AssistantMessage {
        // Unfinished blocks (stream cut) are closed as-is so partial text isn't lost.
        for (i, p) in std::mem::take(&mut self.open) {
            match p.kind {
                BlockKind::Text if !p.text.is_empty() => self.done.push((
                    i,
                    Block::Text {
                        text: p.text,
                        replay: None,
                    },
                )),
                // Unsigned thinking can't be replayed; the adapter drops it on the next request.
                BlockKind::Thinking if !p.text.is_empty() => self.done.push((
                    i,
                    Block::Thinking {
                        text: p.text,
                        replay: None,
                    },
                )),
                _ => {}
            }
        }
        self.done.sort_by_key(|(i, _)| *i);
        AssistantMessage {
            content: std::mem::take(&mut self.done).into_iter().map(|(_, b)| b).collect(),
            stop,
            usage: self.usage,
            provider: self.provider.clone(),
            model: self.model.clone(),
            response_id: self.response_id.clone(),
            error_message,
            thinking_level: self.thinking_level.clone(),
        }
    }

    pub async fn finish(mut self, stop: StopReason) {
        let m = self.message(stop, None);
        self.send(StreamEvent::Done(m)).await;
    }

    pub async fn fail(mut self, kind: ErrorKind, message: impl Into<String>) {
        let stop = if kind == ErrorKind::Aborted {
            StopReason::Aborted
        } else {
            StopReason::Error { kind }
        };
        let m = self.message(stop, Some(message.into()));
        self.send(StreamEvent::Error(m)).await;
    }
}

/// Collect a stream into its final message (the non-streaming call, L2.1).
pub async fn collect(mut rx: mpsc::Receiver<StreamEvent>) -> AssistantMessage {
    while let Some(ev) = rx.recv().await {
        if let StreamEvent::Done(m) | StreamEvent::Error(m) = ev {
            return m;
        }
    }
    AssistantMessage {
        content: vec![],
        stop: StopReason::Error { kind: ErrorKind::Other },
        usage: Usage::default(),
        provider: String::new(),
        model: String::new(),
        response_id: None,
        error_message: Some("stream ended without a final event".into()),
        thinking_level: None,
    }
}
