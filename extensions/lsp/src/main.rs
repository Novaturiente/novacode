//! `novacode-lsp`: a novacode process extension (docs/extensions.md). It listens
//! to `after_tool` and answers with the errors an edit left in the files it
//! touched, as told by real language servers (`lsp.rs`).
//!
//! Protocol: line-delimited JSON-RPC 2.0 on stdin/stdout. Requests may overlap
//! (parallel tool calls), so each is answered from its own task.

mod lsp;

use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;

#[tokio::main]
async fn main() {
    let (out, mut rx) = mpsc::unbounded_channel::<String>();
    tokio::spawn(async move {
        let mut stdout = tokio::io::stdout();
        while let Some(l) = rx.recv().await {
            if stdout.write_all(format!("{l}\n").as_bytes()).await.is_err() {
                break;
            }
            let _ = stdout.flush().await;
        }
    });
    let lsp: Arc<OnceLock<Arc<lsp::Lsp>>> = Arc::default();
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    while let Ok(Some(l)) = lines.next_line().await {
        let Ok(m) = serde_json::from_str::<Value>(&l) else {
            continue;
        };
        if m["method"] == "shutdown" {
            break;
        }
        let id = m["id"].clone();
        if id.is_null() {
            continue;
        }
        let (out, lsp) = (out.clone(), lsp.clone());
        tokio::spawn(async move {
            let result = handle(&m["method"], &m["params"], &lsp).await;
            let _ = out.send(json!({ "jsonrpc": "2.0", "id": id, "result": result }).to_string());
        });
    }
}

async fn handle(method: &Value, p: &Value, lsp: &OnceLock<Arc<lsp::Lsp>>) -> Value {
    match method.as_str() {
        Some("initialize") => {
            let cfg: lsp::Config = serde_json::from_value(p["settings"].clone()).unwrap_or_else(|e| {
                eprintln!("extensions.lsp: {e}; using the defaults");
                lsp::Config::default()
            });
            let wait = cfg.wait_ms;
            let cwd = PathBuf::from(p["cwd"].as_str().unwrap_or("."));
            let logs = PathBuf::from(p["home"].as_str().unwrap_or("/tmp")).join(".novacode/logs");
            let (l, warn) = lsp::Lsp::new(cfg, &cwd, &logs);
            for w in warn {
                eprintln!("{w}");
            }
            let _ = lsp.set(l);
            // The wait plus room for a cold server start.
            json!({ "events": ["after_tool"], "timeoutMs": wait + 5000 })
        }
        Some("event") if p["event"] == "after_tool" => {
            let Some(l) = lsp.get() else { return Value::Null };
            let e = &p["payload"];
            let touched = e["touched"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|v| v.as_str().map(PathBuf::from))
                .collect();
            let tool = e["tool"].as_str().unwrap_or_default();
            match l.after_tool(tool, e["isError"] == true, touched).await {
                // novacode tags appended feedback with the handler's label.
                Some(text) => json!({ "append": format!("diagnostics: {text}") }),
                None => Value::Null,
            }
        }
        _ => Value::Null,
    }
}
