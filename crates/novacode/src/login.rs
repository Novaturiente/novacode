//! Logins, shared by `novacode login` and the TUI's `/login` dialog: subscription
//! OAuth (browser with a loopback redirect, a copied code, or a device code) and
//! API keys. A flow reports [`LoginStep`]s and reads pasted text from `input`.

use anyhow::{Context, bail};
use novacode_ai::auth::{self, AuthStore, antigravity, claude, codex, copilot};
use novacode_ai::catalog::{Catalog, Profile};
use novacode_ai::connect::Providers;
use novacode_tui::dialog::PickItem;
use novacode_tui::login::LoginStep;
use std::collections::BTreeMap;
use std::path::Path;
use tokio::net::TcpListener;
use tokio::sync::mpsc::{self, UnboundedReceiver};

pub use novacode_tui::term::open as open_browser;

pub struct Io {
    pub step: Box<dyn Fn(LoginStep) + Send + Sync>,
    pub input: UnboundedReceiver<String>,
}

/// Ways to log in to `provider`, the default first: (value, label, detail).
pub fn methods(provider: &str) -> &'static [(&'static str, &'static str, &'static str)] {
    match provider {
        claude::PROVIDER => &[
            (
                "browser",
                "Browser",
                "sign in; novacode catches the redirect, nothing to copy",
            ),
            (
                "code",
                "Copy a code",
                "sign in, then paste the code Claude shows (SSH, another machine)",
            ),
        ],
        codex::PROVIDER => &[
            (
                "browser",
                "Browser",
                "sign in; novacode catches the redirect, nothing to copy",
            ),
            (
                "device",
                "Device code",
                "enter a short code at auth.openai.com (SSH, another machine)",
            ),
        ],
        _ => &[],
    }
}

/// Every provider you can log in to, subscriptions first, each marked if already connected.
pub fn providers(p: &Providers) -> Vec<PickItem> {
    let mut v: Vec<&Profile> = p
        .profiles
        .values()
        .filter(|pr| pr.oauth() || (!pr.key_optional && pr.api != novacode_ai::catalog::Api::Process))
        .collect();
    v.sort_by_key(|pr| !pr.oauth());
    v.into_iter()
        .map(|pr| {
            let kind = if pr.oauth() { "subscription" } else { "API key" };
            let on = if p.connected(pr) { " · connected" } else { "" };
            PickItem {
                label: pr.name.clone(),
                detail: format!("{kind}{on}"),
                value: pr.id.clone(),
            }
        })
        .collect()
}

/// Run a subscription login to the end and save it. Returns lines to show.
pub async fn oauth(
    store: &AuthStore,
    provider: &str,
    account: &str,
    method: &str,
    io: &mut Io,
) -> anyhow::Result<Vec<String>> {
    let mut notes = Vec::new();
    let step = |s: LoginStep| (io.step)(s);
    let cred = match (provider, method) {
        (claude::PROVIDER, "code") => {
            let (verifier, challenge) = auth::pkce();
            let url = claude::authorize_url(&verifier, &challenge, claude::COPY_REDIRECT);
            step(LoginStep::Url {
                url: url.clone(),
                note: "Sign in, then paste the code Claude shows.".into(),
                paste: true,
            });
            open_browser(&url);
            let line = io.input.recv().await.context("login cancelled")?;
            claude::exchange(&line, &verifier, claude::COPY_REDIRECT).await?
        }
        (claude::PROVIDER, _) => {
            let (verifier, challenge) = auth::pkce();
            // Any loopback port works for Claude; the fixed one can be forwarded over SSH.
            let listener = match bind(claude::PORT).await {
                Some(l) => Some(l),
                None => bind(0).await,
            };
            let port = listener
                .as_ref()
                .and_then(|l| l.local_addr().ok())
                .map_or(claude::PORT, |a| a.port());
            let redirect = format!("http://localhost:{port}/callback");
            let url = claude::authorize_url(&verifier, &challenge, &redirect);
            step(LoginStep::Url {
                url: url.clone(),
                note: "Sign in in your browser; novacode catches the redirect. On another machine, paste the URL the browser ends on.".into(),
                paste: true,
            });
            open_browser(&url);
            let (code, state) = catch_or_input(listener, &mut io.input).await?;
            if state.as_deref().is_some_and(|s| s != verifier) {
                bail!("OAuth state mismatch; start the login again");
            }
            claude::exchange(&code, &verifier, &redirect).await?
        }
        (codex::PROVIDER, "device") => {
            let d = codex::start().await?;
            step(LoginStep::Device {
                url: codex::VERIFY_URL.into(),
                code: d.user_code.clone(),
            });
            open_browser(codex::VERIFY_URL);
            codex::finish(&d).await?
        }
        (codex::PROVIDER, _) => {
            let (verifier, challenge) = auth::pkce();
            let state = auth::pkce().0;
            let url = codex::authorize_url(&challenge, &state);
            let listener = bind(codex::PORT).await;
            let note = if listener.is_some() {
                "Sign in in your browser; novacode catches the redirect. On another machine, paste the URL the browser ends on."
            } else {
                // The Codex CLI holds the port while it logs in.
                "Port 1455 is busy (a Codex CLI login?). Sign in, then paste the URL the browser ends on."
            };
            step(LoginStep::Url {
                url: url.clone(),
                note: note.into(),
                paste: true,
            });
            open_browser(&url);
            let (code, got) = catch_or_input(listener, &mut io.input).await?;
            if got.as_deref().is_some_and(|s| s != state) {
                bail!("OAuth state mismatch; start the login again");
            }
            codex::exchange(&code, &verifier).await?
        }
        (antigravity::PROVIDER, _) => {
            let (verifier, challenge) = auth::pkce();
            let state = auth::pkce().0;
            let url = antigravity::authorize_url(&challenge, &state);
            step(LoginStep::Url {
                url: url.clone(),
                note: "Sign in with Google; novacode catches the redirect. On another machine, paste the URL the browser ends on.".into(),
                paste: true,
            });
            open_browser(&url);
            let (code, got) = catch_or_input(bind(antigravity::PORT).await, &mut io.input).await?;
            if got.as_deref() != Some(state.as_str()) {
                bail!("OAuth state mismatch; start the login again");
            }
            antigravity::exchange(&code, &verifier).await?
        }
        (copilot::PROVIDER, _) => {
            let d = copilot::start().await?;
            step(LoginStep::Device {
                url: copilot::VERIFY_URL.into(),
                code: d.user_code.clone(),
            });
            open_browser(copilot::VERIFY_URL);
            let c = copilot::finish(&d).await?;
            let on = copilot::enable_models(&c.access).await;
            if !on.is_empty() {
                notes.push(format!("enabled models: {}", on.join(", ")));
            }
            c
        }
        _ => bail!(
            "unknown provider `{provider}` (subscriptions: claude, codex, antigravity, copilot; API keys: see `novacode models --all`)"
        ),
    };
    store.put(provider, account, cred).await?;
    notes.push(if account == auth::DEFAULT_ACCOUNT {
        format!("logged in to {provider}")
    } else {
        format!("logged in to {provider} (account `{account}`)")
    });
    Ok(notes)
}

/// Check an API key with a free model-list call. Ok = the number of models.
pub async fn check_key(
    home: &Path,
    profiles: &BTreeMap<String, Profile>,
    provider: &str,
    key: &str,
) -> Result<usize, String> {
    let p = std::sync::Arc::new(Providers {
        profiles: profiles.clone(),
        catalog: Catalog::default(),
        auth: AuthStore::new(home.join("auth.json")),
        cli_key: Some((provider.into(), key.into())),
        native_search: false,
    });
    tokio::time::timeout(std::time::Duration::from_secs(30), async {
        match p.adapter(provider, None).await {
            Ok((a, _)) => a.list_models().await.map(|ids| ids.len()),
            Err(e) => Err(e.to_string()),
        }
    })
    .await
    .unwrap_or_else(|_| Err("no answer in 30s".into()))
}

/// What the key prompt says for `pr`.
pub fn key_note(pr: &Profile) -> String {
    let mut s = format!(
        "Paste your {} API key (or $VAR / !command to read it on each use).",
        pr.name
    );
    if let Some(env) = pr.env.first() {
        s += &format!(" Without a saved key novacode reads {env}.");
    }
    s
}

/// Lines typed on stdin, for the CLI.
// shortcut: the reader thread stays blocked on stdin after the login; only use it in a process that exits next.
pub fn stdin_lines() -> UnboundedReceiver<String> {
    let (tx, rx) = mpsc::unbounded_channel();
    std::thread::spawn(move || {
        for l in std::io::stdin().lines() {
            let Ok(l) = l else { break };
            if tx.send(l).is_err() {
                break;
            }
        }
    });
    rx
}

/// How the CLI shows a step.
pub fn print_step(s: LoginStep) {
    match s {
        LoginStep::Url { url, note, paste } => {
            eprintln!("{note}\n\n{url}\n");
            if paste {
                eprint!("paste here (or wait for the browser): ");
            }
        }
        LoginStep::Device { url, code } => {
            eprintln!("Open {url} and enter the code\n\n    {code}\n\nWaiting for you to approve (Ctrl+C to cancel)…")
        }
        LoginStep::Key { note } => eprintln!("{note}"),
        LoginStep::Info { text } => eprintln!("{text}"),
    }
}

async fn bind(port: u16) -> Option<TcpListener> {
    TcpListener::bind(("127.0.0.1", port)).await.ok()
}

/// Wait for an OAuth redirect on the listener, or a pasted redirect URL / code,
/// whichever comes first. Returns (code, state).
pub async fn catch_or_input(
    listener: Option<TcpListener>,
    input: &mut UnboundedReceiver<String>,
) -> anyhow::Result<(String, Option<String>)> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let caught = async {
        let Some(l) = listener else {
            return std::future::pending().await;
        };
        loop {
            let Ok((mut sock, _)) = l.accept().await else { continue };
            let mut buf = vec![0u8; 8192];
            let n = sock.read(&mut buf).await.unwrap_or(0);
            let req = String::from_utf8_lossy(&buf[..n]).to_string();
            let target = req.split_whitespace().nth(1).unwrap_or_default().to_string();
            if !target.contains("code=") {
                let _ = sock
                    .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n")
                    .await;
                continue;
            }
            let page = "<html><body><h3>novacode got your sign-in. You can close this tab and go back to the terminal.</h3></body></html>";
            let reply = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{page}",
                page.len()
            );
            let _ = sock.write_all(reply.as_bytes()).await;
            return target;
        }
    };
    let got = tokio::select! {
        t = caught => t,
        l = input.recv() => l.context("login cancelled")?,
    };
    let pasted = got.trim();
    if pasted.starts_with("http") && !pasted.contains("code=") {
        bail!(
            "that URL has no sign-in code: the browser hasn't reached the redirect yet. Finish signing in (approve access), then paste the URL it ends on: it starts with http://localhost and has ?code="
        );
    }
    let (code, state) = claude::parse_pasted(pasted);
    if code.is_empty() {
        bail!("no authorization code received");
    }
    Ok((percent_decode(&code), state.map(|s| percent_decode(&s))))
}

/// `4%2F0Ab` → `4/0Ab` (query values in a redirect URL).
fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match (
            b[i],
            b.get(i + 1..i + 3)
                .and_then(|h| u8::from_str_radix(std::str::from_utf8(h).ok()?, 16).ok()),
        ) {
            (b'%', Some(v)) => {
                out.push(v);
                i += 3;
            }
            (b'+', _) => {
                out.push(b' ');
                i += 1;
            }
            (c, _) => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percent_decoding() {
        assert_eq!(percent_decode("4%2F0Ab-x%3D"), "4/0Ab-x=");
        assert_eq!(percent_decode("plain%zz"), "plain%zz");
    }

    #[tokio::test]
    async fn redirect_or_paste() {
        // A pasted redirect URL wins when nothing calls back.
        let (tx, mut rx) = mpsc::unbounded_channel();
        tx.send("http://localhost:1/callback?code=a%2Fb&state=s1".into())
            .unwrap();
        assert_eq!(
            catch_or_input(None, &mut rx).await.unwrap(),
            ("a/b".into(), Some("s1".into()))
        );
        // The browser's redirect on the loopback port.
        let l = bind(0).await.unwrap();
        let port = l.local_addr().unwrap().port();
        let (_tx, mut rx) = mpsc::unbounded_channel();
        let hit = tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let mut s = tokio::net::TcpStream::connect(("127.0.0.1", port)).await.unwrap();
            s.write_all(b"GET /callback?code=xyz&state=s2 HTTP/1.1\r\n\r\n")
                .await
                .unwrap();
            let mut page = String::new();
            let _ = s.read_to_string(&mut page).await;
            page
        });
        assert_eq!(
            catch_or_input(Some(l), &mut rx).await.unwrap(),
            ("xyz".into(), Some("s2".into()))
        );
        assert!(hit.await.unwrap().contains("got your sign-in"));
        // Closing the input (Esc in the dialog) cancels.
        let (tx, mut rx) = mpsc::unbounded_channel::<String>();
        drop(tx);
        assert!(catch_or_input(None, &mut rx).await.is_err());
        // A provider's sign-in page (no code yet) gets a clear error, not a state mismatch.
        let (tx, mut rx) = mpsc::unbounded_channel::<String>();
        tx.send("https://accounts.google.com/signin/oauth/v3/consent?authuser=0#".into())
            .unwrap();
        let e = catch_or_input(None, &mut rx).await.unwrap_err().to_string();
        assert!(e.contains("no sign-in code"), "{e}");
    }
}
