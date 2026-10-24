//! The one send-and-stream loop every SSE adapter shares (L2.2, L2.18):
//! retry before any content, idle timeout, cancel, trace. An adapter supplies
//! only how to classify an HTTP error and how to read one event ([`Wire`]).

use crate::adapter::{Builder, EVENT_BUFFER};
use crate::retry;
use crate::sse::{SseEvent, SseParser};
use crate::trace::Trace;
use futures_util::StreamExt;
use novacode_types::{ErrorKind, StopReason, StreamEvent};
use reqwest::header::HeaderMap;
use serde_json::Value;
use std::future::Future;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
/// Time allowed for the response headers, and between any two body chunks.
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(120);

pub type Failure = (ErrorKind, String);

pub fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .tcp_keepalive(Duration::from_secs(30))
        .build()
        .unwrap_or_default()
}

/// One wire format's stream reader. A fresh one is made per attempt.
pub trait Wire: Send {
    /// Map a non-2xx response. Text matching is allowed here only (L2.19).
    fn classify(status: u16, headers: &HeaderMap, body: &str) -> Failure;
    /// Read one event; `Some(stop)` ends the stream.
    fn on_event(
        &mut self,
        ev: &SseEvent,
        b: &mut Builder,
    ) -> impl Future<Output = Result<Option<StopReason>, Failure>> + Send;
    /// The body ended without an end event.
    fn at_eof(&mut self, b: &mut Builder) -> impl Future<Output = Result<StopReason, Failure>> + Send;
}

/// Spawn the request and return its event stream (never fails, L2.1).
pub fn drive<W: Wire + 'static>(
    mut b: Builder,
    rx: mpsc::Receiver<StreamEvent>,
    request: reqwest::RequestBuilder,
    body: Vec<u8>,
    trace: Option<Trace>,
    cancel: CancellationToken,
    new_wire: impl Fn() -> W + Send + Sync + 'static,
) -> mpsc::Receiver<StreamEvent> {
    tokio::spawn(async move {
        if let Some(t) = &trace {
            t.request(&request, &body);
        }
        let label = b.provider().to_string();
        tokio::select! {
            _ = cancel.cancelled() => b.fail(ErrorKind::Aborted, "aborted").await,
            r = run(request, &mut b, trace.as_ref(), &new_wire, &label) => match r {
                Ok(stop) => b.finish(stop).await,
                Err((kind, msg)) => b.fail(kind, msg).await,
            },
        }
    });
    rx
}

/// A builder plus its receiver.
pub fn channel(provider: &str, model: &str) -> (Builder, mpsc::Receiver<StreamEvent>) {
    let (tx, rx) = mpsc::channel(EVENT_BUFFER);
    (Builder::new(tx, provider, model), rx)
}

async fn run<W: Wire>(
    request: reqwest::RequestBuilder,
    b: &mut Builder,
    trace: Option<&Trace>,
    new_wire: &impl Fn() -> W,
    label: &str,
) -> Result<StopReason, Failure> {
    let mut attempt = 0;
    loop {
        let Some(req) = request.try_clone() else {
            return Err((ErrorKind::Other, "request not cloneable".into()));
        };
        let (status, headers, f) = match send(req, IDLE_TIMEOUT).await {
            Err(f) => (None, None, f),
            Ok(resp) if !resp.status().is_success() => {
                let status = resp.status().as_u16();
                let headers = resp.headers().clone();
                let text = resp.text().await.unwrap_or_default();
                if let Some(t) = trace {
                    t.response_error(status, &text);
                }
                let f = W::classify(status, &headers, &text);
                (Some(status), Some(headers), f)
            }
            Ok(resp) => match stream_body(resp, b, trace, new_wire()).await {
                Ok(stop) => return Ok(stop),
                // Mid-stream failures are not retried here; the loop decides (L4.6).
                Err(f) if b.started => return Err(f),
                Err(f) => (None, None, f),
            },
        };
        if !retry::should_retry(status, headers.as_ref()) || !f.0.is_retryable() {
            return Err(f);
        }
        let server = headers.as_ref().and_then(retry::retry_after);
        let Some(d) = retry::delay(attempt, server) else {
            return Err(f);
        };
        tracing::warn!(attempt, ?d, error = %f.1, "{label}: retrying");
        b.rollback().await;
        tokio::time::sleep(d).await;
        attempt += 1;
    }
}

/// Send and wait for the response headers; a server that never answers is a
/// transient failure, so the retry path handles it.
async fn send(req: reqwest::RequestBuilder, limit: Duration) -> Result<reqwest::Response, Failure> {
    match tokio::time::timeout(limit, req.send()).await {
        Err(_) => Err((ErrorKind::Transient, format!("no response headers within {limit:?}"))),
        Ok(Err(e)) => Err((ErrorKind::Transient, format!("network error: {e}"))),
        Ok(Ok(resp)) => Ok(resp),
    }
}

async fn stream_body<W: Wire>(
    resp: reqwest::Response,
    b: &mut Builder,
    trace: Option<&Trace>,
    mut wire: W,
) -> Result<StopReason, Failure> {
    let mut body = resp.bytes_stream();
    let mut sse = SseParser::default();
    loop {
        let chunk = match tokio::time::timeout(IDLE_TIMEOUT, body.next()).await {
            Err(_) => return Err((ErrorKind::Transient, "stream idle timeout".into())),
            Ok(None) => break,
            Ok(Some(Err(e))) => return Err((ErrorKind::Transient, format!("stream error: {e}"))),
            Ok(Some(Ok(c))) => c,
        };
        for ev in sse.push(&chunk) {
            if let Some(t) = trace {
                t.sse(&ev);
            }
            if let Some(stop) = wire.on_event(&ev, b).await? {
                return Ok(stop);
            }
        }
    }
    if let Some(ev) = sse.finish() {
        if let Some(t) = trace {
            t.sse(&ev);
        }
        if let Some(stop) = wire.on_event(&ev, b).await? {
            return Ok(stop);
        }
    }
    wire.at_eof(b).await
}

/// Error kind for OpenAI-style bodies (`{"error": {"message", "type", "code"}}`),
/// used by the OpenAI-shaped adapters and most compatible vendors.
pub fn classify_openai(status: u16, headers: &HeaderMap, body: &str) -> Failure {
    let v: Value = serde_json::from_str(body).unwrap_or_default();
    // Some vendors send a list of errors, some a bare object.
    let e = match &v {
        Value::Array(a) => a.first().map(|x| x["error"].clone()).unwrap_or_default(),
        _ => v["error"].clone(),
    };
    let msg = e["message"]
        .as_str()
        .or_else(|| e.as_str())
        .map(String::from)
        .unwrap_or_else(|| format!("HTTP {status}: {}", body.chars().take(500).collect::<String>()));
    let code = e["code"]
        .as_str()
        .or(e["type"].as_str())
        .or(e["status"].as_str())
        .unwrap_or_default();
    (openai_kind(status, code, &msg, headers), msg)
}

fn openai_kind(status: u16, code: &str, msg: &str, headers: &HeaderMap) -> ErrorKind {
    let lower = msg.to_ascii_lowercase();
    if code == "context_length_exceeded"
        || lower.contains("context length")
        || lower.contains("context window")
        || lower.contains("maximum context")
        || lower.contains("too many tokens")
        || lower.contains("prompt is too long")
        || lower.contains("input is too long")
    {
        return ErrorKind::Overflow;
    }
    // "Quota exceeded" on a plain 429 is often a per-minute limit: only payment
    // statuses or daily wording mean the quota is used up. "billing" alone does not:
    // Google's free-tier per-minute 429 says "check your plan and billing details".
    let used_up = matches!(status, 402 | 403) || ["per day", "daily"].iter().any(|w| lower.contains(w));
    if code == "insufficient_quota"
        || lower.contains("insufficient credits")
        || (lower.contains("quota exceeded") && used_up)
    {
        return ErrorKind::QuotaExhausted;
    }
    match status {
        401 | 403 => ErrorKind::Auth,
        402 => ErrorKind::QuotaExhausted,
        429 => ErrorKind::RateLimit {
            retry_after_ms: retry::retry_after(headers).map(|d| d.as_millis() as u64),
        },
        408 | 409 | 500.. => ErrorKind::Transient,
        400 | 404 | 413 | 422 => ErrorKind::InvalidRequest,
        _ => ErrorKind::Other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn openai_errors_map_to_kinds() {
        let h = HeaderMap::new();
        let k = |s, b: &str| classify_openai(s, &h, b).0;
        assert_eq!(
            k(
                400,
                r#"{"error":{"message":"This model's maximum context length is 128000 tokens","code":"context_length_exceeded"}}"#
            ),
            ErrorKind::Overflow
        );
        assert_eq!(
            k(
                429,
                r#"{"error":{"message":"You exceeded your current quota","code":"insufficient_quota"}}"#
            ),
            ErrorKind::QuotaExhausted
        );
        assert_eq!(
            k(429, r#"{"error":{"message":"slow down"}}"#),
            ErrorKind::RateLimit { retry_after_ms: None }
        );
        assert_eq!(k(401, "nope"), ErrorKind::Auth);
        assert_eq!(k(503, ""), ErrorKind::Transient);
        assert_eq!(
            k(
                400,
                r#"[{"error":{"code":400,"message":"bad field","status":"INVALID_ARGUMENT"}}]"#
            ),
            ErrorKind::InvalidRequest
        );
        assert_eq!(
            classify_openai(400, &h, r#"{"error":{"message":"bad field"}}"#).1,
            "bad field"
        );
        assert_eq!(
            k(429, r#"{"error":{"message":"Quota exceeded for requests per minute"}}"#),
            ErrorKind::RateLimit { retry_after_ms: None },
            "a per-minute quota is a rate limit"
        );
        assert_eq!(
            k(429, r#"{"error":{"message":"Quota exceeded for requests per day"}}"#),
            ErrorKind::QuotaExhausted
        );
        assert_eq!(
            k(403, r#"{"error":{"message":"Quota exceeded"}}"#),
            ErrorKind::QuotaExhausted
        );
        assert_eq!(
            k(
                429,
                r#"{"error":{"message":"Quota exceeded, please check your plan and billing details"}}"#
            ),
            ErrorKind::RateLimit { retry_after_ms: None },
            "billing wording alone is a rate limit"
        );
    }

    #[tokio::test]
    async fn silent_server_times_out_as_transient() {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", l.local_addr().unwrap());
        // Accept and hold the connection without ever answering.
        tokio::spawn(async move {
            let (_s, _) = l.accept().await.unwrap();
            std::future::pending::<()>().await
        });
        let req = client().post(url).body("x");
        let (kind, msg) = send(req, Duration::from_millis(200)).await.unwrap_err();
        assert_eq!(kind, ErrorKind::Transient);
        assert!(msg.contains("no response headers"), "{msg}");
    }
}
