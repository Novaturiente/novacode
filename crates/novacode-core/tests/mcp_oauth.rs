//! MCP OAuth end to end against a fake server: discovery, dynamic registration,
//! PKCE login, an authorized call, then a revoked token recovered by refresh.

use novacode_core::{mcp, mcp_oauth};
use novacode_tools::ToolCtx;
use serde_json::json;
use std::io::BufRead;
use std::sync::Arc;

struct Server(std::process::Child);
impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
    }
}

fn ctx(dir: &std::path::Path) -> ToolCtx {
    ToolCtx {
        cwd: dir.into(),
        env: Arc::new(novacode_tools::LocalEnv),
        cancel: Default::default(),
        updates: None,
        files: Default::default(),
        session_id: "t".into(),
        processes: None,
        native_tools: false,
        nested: None,
    }
}

async fn call(home: &std::path::Path, url: &str) -> (bool, String) {
    let cfg = mcp::Configured {
        name: "fake".into(),
        config: mcp::ServerConfig {
            url: Some(url.into()),
            ..Default::default()
        },
        source: home.join("mcp.json"),
    };
    let hub = mcp::Hub::start(vec![cfg], home, home);
    hub.settle().await;
    let status = format!("{:?}", hub.status());
    match hub.tools().first() {
        Some(t) => {
            let out = t.execute(json!({}), &ctx(home)).await;
            let text: String = out
                .content
                .iter()
                .filter_map(|b| match b {
                    novacode_types::Block::Text { text, .. } => Some(text.as_str()),
                    _ => None,
                })
                .collect();
            (!out.is_error, format!("{status} {text}"))
        }
        None => (false, status),
    }
}

#[tokio::test]
async fn login_call_and_refresh() {
    let mut child = std::process::Command::new("python3")
        .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/oauth_mcp.py"))
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut port = String::new();
    std::io::BufReader::new(child.stdout.take().unwrap())
        .read_line(&mut port)
        .unwrap();
    let _srv = Server(child);
    let base = format!("http://127.0.0.1:{}", port.trim());
    let url = format!("{base}/mcp");
    let home = tempfile::tempdir().unwrap();

    // Not logged in: the error says how to log in.
    let (ok, msg) = call(home.path(), &url).await;
    assert!(!ok && msg.contains("novacode mcp login fake"), "{msg}");

    // Log in: discover, register, then follow the authorize redirect like a browser.
    let redirect = "http://127.0.0.1:1/callback";
    let p = mcp_oauth::begin(&url, &Default::default(), redirect).await.unwrap();
    let no_follow = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    let loc = no_follow.get(&p.url).send().await.unwrap().headers()["location"]
        .to_str()
        .unwrap()
        .to_string();
    let q: std::collections::HashMap<String, String> =
        reqwest::Url::parse(&loc).unwrap().query_pairs().into_owned().collect();
    assert_eq!(q["state"], p.state);
    mcp_oauth::finish(home.path(), "fake", p, &q["code"]).await.unwrap();
    let mode = std::fs::metadata(mcp_oauth::store_path(home.path())).unwrap();
    assert_eq!(
        std::os::unix::fs::PermissionsExt::mode(&mode.permissions()) & 0o777,
        0o600
    );

    let (ok, msg) = call(home.path(), &url).await;
    assert!(ok && msg.contains("token at1"), "{msg}");

    // The server drops the token: a 401 is answered with one refresh and a retry.
    reqwest::Client::new()
        .post(format!("{base}/revoke"))
        .send()
        .await
        .unwrap();
    let (ok, msg) = call(home.path(), &url).await;
    assert!(ok && msg.contains("token at2"), "{msg}");

    // Logged out: back to the hint.
    assert!(mcp_oauth::remove(home.path(), "fake").await.unwrap());
    let (ok, msg) = call(home.path(), &url).await;
    assert!(!ok && msg.contains("mcp login"), "{msg}");
}
