//! `novacode-image-gen`: a novacode process extension (docs/extensions.md) that
//! adds the `generate_image` tool. Line-delimited JSON-RPC 2.0 on stdio; each
//! request is answered from its own task, so several images can be drawn at once.

mod image;

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
    let tool: Arc<OnceLock<image::GenerateImage>> = Arc::default();
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
        let (out, tool) = (out.clone(), tool.clone());
        tokio::spawn(async move {
            let p = &m["params"];
            let result = match m["method"].as_str() {
                Some("initialize") => {
                    // NOVACODE_HOME is inherited from novacode; else ~/.novacode.
                    let home = std::env::var_os("NOVACODE_HOME")
                        .map(PathBuf::from)
                        .unwrap_or_else(|| PathBuf::from(p["home"].as_str().unwrap_or_default()).join(".novacode"));
                    let key = p["settings"]["apiKey"]
                        .as_str()
                        .map(String::from)
                        .or_else(|| std::env::var("GEMINI_API_KEY").ok())
                        .or_else(|| std::env::var("GOOGLE_API_KEY").ok());
                    let _ = tool.set(image::GenerateImage {
                        auth: novacode_ai::auth::AuthStore::new(home.join("auth.json")),
                        key,
                    });
                    json!({ "tools": [image::declaration()] })
                }
                Some("tool") if p["name"] == "generate_image" => match tool.get() {
                    Some(t) => {
                        t.run(&p["args"], &PathBuf::from(p["cwd"].as_str().unwrap_or(".")))
                            .await
                    }
                    None => json!({ "content": "not initialized", "isError": true }),
                },
                _ => Value::Null,
            };
            let _ = out.send(json!({ "jsonrpc": "2.0", "id": id, "result": result }).to_string());
        });
    }
}
