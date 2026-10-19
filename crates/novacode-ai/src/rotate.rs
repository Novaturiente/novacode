//! Live subscription adapter (L2.13, L2.14). Before each request it reads the
//! account's current credential, refreshing it when due, and reuses the built
//! adapter while the token is unchanged. When an account is exhausted
//! (quota, or a rate limit longer than the retry policy waits) before any
//! output, it moves on to the provider's next logged-in account and stays
//! there. It never falls back to a metered API key.

use crate::adapter::{Adapter, Builder, EVENT_BUFFER, Request};
use crate::connect::Providers;
use crate::retry;
use crate::trace::Trace;
use futures_util::future::BoxFuture;
use novacode_types::{ErrorKind, StopReason, StreamEvent};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

pub struct Live {
    inner: Arc<Inner>,
}

struct Inner {
    providers: Arc<Providers>,
    provider: String,
    accounts: Vec<String>,
    current: AtomicUsize,
    trace: Option<Trace>,
    cached: Mutex<Option<Built>>,
}

/// (account index, access token, adapter built with it).
type Built = (usize, String, Arc<dyn Adapter>);

impl Live {
    pub fn new(
        providers: Arc<Providers>,
        provider: &str,
        accounts: Vec<String>,
        trace: Option<Trace>,
        first: Arc<dyn Adapter>,
    ) -> Self {
        let accounts = if accounts.is_empty() {
            vec![crate::auth::DEFAULT_ACCOUNT.into()]
        } else {
            accounts
        };
        let token = providers.auth.cached_access(provider, &accounts[0]).unwrap_or_default();
        Self {
            inner: Arc::new(Inner {
                provider: provider.into(),
                accounts,
                current: AtomicUsize::new(0),
                trace,
                cached: Mutex::new(Some((0, token, first))),
                providers,
            }),
        }
    }

    /// The account requests go to now.
    pub fn account(&self) -> String {
        self.inner.accounts[self.inner.current.load(Ordering::Relaxed)].clone()
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// 374685000 → "4d 8h"; 5400000 → "1h 30m"; 90000 → "1m".
fn human(ms: u64) -> String {
    let m = ms / 60_000;
    let (d, h, m) = (m / 1440, m / 60 % 24, m % 60);
    match (d, h) {
        (0, 0) => format!("{}m", m.max(1)),
        (0, _) => format!("{h}h {m}m"),
        _ => format!("{d}d {h}h"),
    }
}

/// An account is used up: rotating beats waiting.
fn exhausted(stop: &StopReason) -> bool {
    match stop {
        StopReason::Error {
            kind: ErrorKind::QuotaExhausted,
        } => true,
        StopReason::Error {
            kind: ErrorKind::RateLimit {
                retry_after_ms: Some(ms),
            },
        } => retry::delay(0, Some(Duration::from_millis(*ms))).is_none(),
        _ => false,
    }
}

impl Inner {
    async fn adapter(&self, i: usize) -> Result<Arc<dyn Adapter>, String> {
        let account = &self.accounts[i];
        let cred = self
            .providers
            .auth
            .credential(&self.provider, account)
            .await
            .map_err(|e| e.to_string())?;
        if let Some((ci, tok, a)) = self.cached.lock().unwrap().as_ref()
            && *ci == i
            && *tok == cred.access
        {
            return Ok(a.clone());
        }
        let (a, _) = self
            .providers
            .adapter_for(&self.provider, account, self.trace.clone())
            .await
            .map_err(|e| e.to_string())?;
        *self.cached.lock().unwrap() = Some((i, cred.access, a.clone()));
        Ok(a)
    }

    async fn run(&self, req: Request, cancel: CancellationToken, tx: mpsc::Sender<StreamEvent>) {
        let n = self.accounts.len();
        'accounts: for tried in 0..n {
            let i = self.current.load(Ordering::Relaxed);
            let a = match self.adapter(i).await {
                Ok(a) => a,
                Err(e) => {
                    let msg = format!("{} account `{}`: {e}", self.provider, self.accounts[i]);
                    return Builder::new(tx, &self.provider, &req.model)
                        .fail(ErrorKind::Auth, msg)
                        .await;
                }
            };
            let mut rx = a.stream(req.clone(), cancel.clone());
            let mut forwarded = false;
            while let Some(mut ev) = rx.recv().await {
                if let StreamEvent::Error(m) = &ev
                    && !forwarded
                    && tried + 1 < n
                    && exhausted(&m.stop)
                {
                    let next = (i + 1) % n;
                    let why = m.error_message.as_deref().unwrap_or("used up");
                    let note = format!(
                        "{} account `{}` is exhausted ({why}); switched to account `{}`",
                        self.provider, self.accounts[i], self.accounts[next]
                    );
                    if tx.send(StreamEvent::Notice { text: note }).await.is_err() {
                        return;
                    }
                    tracing::warn!(
                        provider = %self.provider,
                        from = %self.accounts[i],
                        to = %self.accounts[next],
                        "account exhausted, rotating"
                    );
                    let wait = match &m.stop {
                        StopReason::Error {
                            kind:
                                ErrorKind::RateLimit {
                                    retry_after_ms: Some(ms),
                                },
                        } => *ms,
                        _ => 3_600_000, // quota without a reset time: try again in an hour
                    };
                    self.providers
                        .auth
                        .mark_exhausted(&self.provider, &self.accounts[i], now_ms() + wait);
                    self.current.store(next, Ordering::Relaxed);
                    continue 'accounts;
                }
                // Last account out too: say when it resets and how to add one.
                if let StreamEvent::Error(m) = &mut ev
                    && !forwarded
                    && exhausted(&m.stop)
                {
                    let wait = match &m.stop {
                        StopReason::Error {
                            kind:
                                ErrorKind::RateLimit {
                                    retry_after_ms: Some(ms),
                                },
                        } => format!("resets in {}", human(*ms)),
                        _ => "quota used up".into(),
                    };
                    m.error_message = Some(format!(
                        "{} account `{}`: {} ({wait}). Add another account with `novacode login {} --account <name>`, or switch model with /model.",
                        self.provider,
                        self.accounts[i],
                        m.error_message.as_deref().unwrap_or("rate limited"),
                        self.provider
                    ));
                }
                forwarded = true;
                if tx.send(ev).await.is_err() {
                    return;
                }
            }
            return;
        }
    }
}

impl Adapter for Live {
    fn native_tool_changes(&self, model: &str) -> bool {
        let built = self.inner.cached.lock().unwrap().as_ref().map(|b| b.2.clone());
        built.is_some_and(|a| a.native_tool_changes(model))
    }

    fn warm_ttl(&self, req: &Request) -> Option<std::time::Duration> {
        let built = self.inner.cached.lock().unwrap().as_ref().map(|b| b.2.clone());
        built.and_then(|a| a.warm_ttl(req))
    }

    fn stream(&self, req: Request, cancel: CancellationToken) -> mpsc::Receiver<StreamEvent> {
        let (tx, rx) = mpsc::channel(EVENT_BUFFER);
        let inner = self.inner.clone();
        tokio::spawn(async move { inner.run(req, cancel, tx).await });
        rx
    }

    fn list_models(&self) -> BoxFuture<'_, Result<Vec<String>, String>> {
        Box::pin(async move {
            let a = self.inner.adapter(self.inner.current.load(Ordering::Relaxed)).await?;
            a.list_models().await
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter::{CacheRetention, collect};
    use crate::auth::OAuthCredential;
    use novacode_base::settings::Settings;
    use novacode_types::Message;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// A server that answers `replies` in order, one per connection.
    async fn server(replies: Vec<String>) -> String {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", l.local_addr().unwrap());
        tokio::spawn(async move {
            for r in replies {
                let (mut s, _) = l.accept().await.unwrap();
                let mut buf = vec![0u8; 65536];
                let _ = s.read(&mut buf).await;
                s.write_all(r.as_bytes()).await.unwrap();
                let _ = s.shutdown().await;
            }
        });
        url
    }

    fn http(status: &str, ctype: &str, body: &str) -> String {
        format!(
            "HTTP/1.1 {status}\r\ncontent-type: {ctype}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    #[tokio::test]
    async fn rotates_to_the_next_account_and_stays() {
        let ok = http(
            "200 OK",
            "text/event-stream",
            "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n",
        );
        let quota = http(
            "429 Too Many Requests",
            "application/json",
            r#"{"error":{"code":"insufficient_quota","message":"quota"}}"#,
        );
        let url = server(vec![quota, ok.clone(), ok]).await;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let sub = format!(
            r#"{{ "providers": {{ "sub": {{ "name": "Sub", "api": "openai-chat", "auth": "oauth", "baseUrl": "{url}" }} }} }}"#
        );
        std::fs::write(&path, sub).unwrap();
        let p = Arc::new(Providers::load(dir.path(), &Settings::load(&path, None, false)));
        let cred = |t: &str| OAuthCredential {
            access: t.into(),
            refresh: "r".into(),
            expires_ms: u64::MAX / 2,
            project: None,
        };
        p.auth.put("sub", "default", cred("a")).await.unwrap();
        p.auth.put("sub", "work", cred("b")).await.unwrap();
        let (first, _) = p.adapter_for("sub", "default", None).await.unwrap();
        let a = Live::new(p.clone(), "sub", p.auth.accounts("sub"), None, first);
        assert_eq!(a.account(), "default");
        let req = Request {
            tail: Vec::new(),
            model: "m".into(),
            messages: vec![Message::user("hi")],
            tools: vec![],
            max_tokens: 10,
            cache: CacheRetention::Short,
            thinking: None,
            images: false,
            session: None,
        };
        let mut rx = a.stream(req.clone(), CancellationToken::new());
        let first = rx.recv().await.unwrap();
        assert!(
            matches!(&first, StreamEvent::Notice { text: n } if n.contains("`default` is exhausted") && n.contains("`work`")),
            "{first:?}"
        );
        let m = collect(rx).await;
        assert_eq!(
            (m.stop, a.account()),
            (StopReason::Stop, "work".to_string()),
            "{:?}",
            m.error_message
        );
        // Another process starts on `work`: the exhaustion was saved.
        assert_eq!(p.auth.accounts("sub"), ["work", "default"]);
        // The next request goes straight to `work`: no notice, no quota error.
        let m = collect(a.stream(req.clone(), CancellationToken::new())).await;
        assert_eq!(
            (m.stop, a.account()),
            (StopReason::Stop, "work".to_string()),
            "rotated once and stayed"
        );
    }

    #[test]
    fn exhaustion_rules() {
        let rl = |ms| StopReason::Error {
            kind: ErrorKind::RateLimit {
                retry_after_ms: Some(ms),
            },
        };
        assert!(exhausted(&StopReason::Error {
            kind: ErrorKind::QuotaExhausted
        }));
        assert!(exhausted(&rl(3_600_000)), "an hour-long limit rotates");
        assert!(!exhausted(&rl(2_000)), "a short limit waits");
        assert_eq!(human(374_685_000), "4d 8h");
        assert_eq!(human(5_400_000), "1h 30m");
        assert_eq!(human(1_000), "1m");
    }
}
