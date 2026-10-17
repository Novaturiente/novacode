//! Scripted provider for tests (L0.9): replies with prepared blocks, streamed
//! in small chunks through the shared builder, and records every request.

use crate::adapter::{Adapter, Builder, EVENT_BUFFER, Request};
use novacode_types::{Block, BlockKind, ErrorKind, StopReason, StreamEvent, Usage};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone)]
pub enum FauxReply {
    Message {
        blocks: Vec<Block>,
        stop: StopReason,
        usage: Usage,
    },
    Fail(ErrorKind, String),
}

impl FauxReply {
    pub fn text(t: &str) -> Self {
        FauxReply::Message {
            blocks: vec![Block::text(t)],
            stop: StopReason::Stop,
            usage: Usage::default(),
        }
    }
}

#[derive(Default, Clone)]
pub struct Faux {
    replies: Arc<Mutex<std::collections::VecDeque<FauxReply>>>,
    /// Requests seen, in order.
    pub requests: Arc<Mutex<Vec<Request>>>,
}

impl Faux {
    pub fn new(replies: impl IntoIterator<Item = FauxReply>) -> Self {
        Self {
            replies: Arc::new(Mutex::new(replies.into_iter().collect())),
            ..Default::default()
        }
    }
}

const CHUNK: usize = 4;

impl Adapter for Faux {
    fn stream(&self, req: Request, cancel: CancellationToken) -> mpsc::Receiver<StreamEvent> {
        let (tx, rx) = mpsc::channel(EVENT_BUFFER);
        let reply = self.replies.lock().unwrap().pop_front();
        let model = req.model.clone();
        self.requests.lock().unwrap().push(req);
        tokio::spawn(async move {
            let mut b = Builder::new(tx, "faux", &model);
            let (blocks, stop, usage) = match reply {
                None => return b.fail(ErrorKind::Other, "faux: no scripted reply left").await,
                Some(FauxReply::Fail(k, m)) => return b.fail(k, m).await,
                Some(FauxReply::Message { blocks, stop, usage }) => (blocks, stop, usage),
            };
            for (i, block) in blocks.into_iter().enumerate() {
                if cancel.is_cancelled() {
                    return b.fail(ErrorKind::Aborted, "aborted").await;
                }
                let (kind, text, id, name) = match &block {
                    Block::Text { text, .. } => (BlockKind::Text, text.clone(), "", ""),
                    Block::Thinking { text, .. } => (BlockKind::Thinking, text.clone(), "", ""),
                    Block::ToolCall(c) => (
                        BlockKind::ToolCall,
                        c.arguments.to_string(),
                        c.id.as_str(),
                        c.name.as_str(),
                    ),
                    _ => continue,
                };
                b.start(i, kind, id, name).await;
                let chars: Vec<char> = text.chars().collect();
                for chunk in chars.chunks(CHUNK) {
                    b.delta(i, &chunk.iter().collect::<String>()).await;
                }
                b.end(i).await;
            }
            b.usage(usage).await;
            b.finish(stop).await;
        });
        rx
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter::{CacheRetention, collect};
    use novacode_types::{Message, ToolCall};
    use serde_json::json;

    fn req() -> Request {
        Request {
            model: "m".into(),
            messages: vec![Message::user("hi")],
            tools: vec![],
            max_tokens: 10,
            cache: CacheRetention::Short,
        }
    }

    #[tokio::test]
    async fn streams_and_collects() {
        let call = ToolCall {
            id: "c1".into(),
            name: "read".into(),
            arguments: json!({"path": "a"}),
            arguments_error: None,
            replay: None,
        };
        let faux = Faux::new([
            FauxReply::Message {
                blocks: vec![Block::text("hello world"), Block::ToolCall(call.clone())],
                stop: StopReason::ToolUse,
                usage: Usage {
                    input: 3,
                    output: 2,
                    ..Default::default()
                },
            },
            FauxReply::Fail(ErrorKind::Overflow, "too long".into()),
        ]);
        let mut rx = faux.stream(req(), CancellationToken::new());
        let mut deltas = String::new();
        let mut done = None;
        while let Some(ev) = rx.recv().await {
            match ev {
                StreamEvent::Delta { index: 0, text } => deltas.push_str(&text),
                StreamEvent::Done(m) => done = Some(m),
                _ => {}
            }
        }
        let m = done.unwrap();
        assert_eq!(deltas, "hello world");
        assert_eq!(m.text(), "hello world");
        assert_eq!(m.tool_calls().next(), Some(&call));
        assert_eq!(m.usage.input, 3);
        let err = collect(faux.stream(req(), CancellationToken::new())).await;
        assert_eq!(
            err.stop,
            StopReason::Error {
                kind: ErrorKind::Overflow
            }
        );
        assert_eq!(faux.requests.lock().unwrap().len(), 2);
    }
}
