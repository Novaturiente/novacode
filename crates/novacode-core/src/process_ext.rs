//! Process extensions (L7.2 tier 2): a long-running program in any language,
//! speaking JSON-RPC 2.0 over stdin/stdout, one message per line.
//!
//! novacode → extension (requests):
//! - `initialize {apiVersion, cwd, session}` → `{tools: [{name, description, parameters, exposure?}],
//!   commands: [{name, description}], events: ["before_tool", "agent_end", …]}`
//! - `tool {name, args, cwd, session}` → a string, or `{content, isError}`
//! - `command {name, args}` → text to show
//! - `event {event, payload}` → for `before_tool` `{block}`/`{args}`, for `after_tool` feedback text
//!
//! - for `input` `{text}`/`{handled}`, for `before_request` text (or a list) appended to that request only
//!
//! extension → novacode (notifications): `notify {text}`, `remind {text, wake}` (a saved note for the model),
//! `ui {op, …}` (a UI op, L9 UI API), `log {text}`. Keys come back as the `ui_key` event.
//!
//! extension → novacode (requests): `host {op, args}`, any operation a script extension has
//! (`exec`, `readFile`, `messages`, `complete`, `spawnAgent`, `settings`, `stateAppend`, …).
//!
//! `initialize` also gets `home` and `settings` (`extensions.<name>`); its result may set
//! `timeoutMs` (event handlers, up to 60 s). Before the process is killed it gets a
//! `shutdown` notification and 2 s to exit.

use futures_util::future::BoxFuture;
use novacode_script::ext::{Backend, CommandReg, Failure, Registration, ToolReg};
use novacode_types::{AgentEvent, StreamEvent};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot};

#[derive(Debug, Clone, serde::Deserialize, Default)]
pub struct Config {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value, String>>>>>;

pub struct ProcessExt {
    name: String,
    /// Event handler timeout asked for in `initialize`.
    pub timeout: Option<Duration>,
    reg: Registration,
    /// Handler id → (kind, name).
    routes: Vec<(&'static str, String)>,
    tx: mpsc::UnboundedSender<String>,
    pending: Pending,
    ids: AtomicU64,
    pid: Option<u32>,
}

impl Drop for ProcessExt {
    fn drop(&mut self) {
        let _ = self
            .tx
            .send(json!({ "jsonrpc": "2.0", "method": "shutdown", "params": {} }).to_string());
        if let Some(pid) = self.pid {
            // shortcut: a detached thread; if novacode exits first the child only gets stdin EOF, which is enough for a well-behaved one.
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_secs(2));
                novacode_tools::process::kill_group_now(pid);
            });
        }
    }
}

impl ProcessExt {
    pub async fn start(
        name: &str,
        c: &Config,
        cwd: &Path,
        session: &str,
        log: &Path,
        host: Arc<crate::extensions::Host>,
    ) -> Result<Arc<ProcessExt>, String> {
        use novacode_script::ext::ExtHost as _;
        let sink = host.sink();
        let mut p = tokio::process::Command::new(&c.command);
        p.args(&c.args)
            .envs(&c.env)
            .env(crate::hooks::DISABLED_ENV, "1")
            .current_dir(cwd)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .process_group(0)
            .kill_on_drop(true);
        if let Some(d) = log.parent() {
            let _ = std::fs::create_dir_all(d);
        }
        p.stderr(
            std::fs::File::create(log)
                .map(std::process::Stdio::from)
                .unwrap_or_else(|_| std::process::Stdio::null()),
        );
        let mut child = p.spawn().map_err(|e| format!("could not start `{}`: {e}", c.command))?;
        let pid = child.id();
        let mut stdin = child.stdin.take().ok_or("no stdin")?;
        let stdout = child.stdout.take().ok_or("no stdout")?;
        let (tx, mut rx) = mpsc::unbounded_channel::<String>();
        tokio::spawn(async move {
            while let Some(l) = rx.recv().await {
                if stdin.write_all(format!("{l}\n").as_bytes()).await.is_err() {
                    break;
                }
                let _ = stdin.flush().await;
            }
        });
        let pending: Pending = Arc::default();
        let (p2, n2, tx2, host2) = (pending.clone(), name.to_string(), tx.clone(), host.clone());
        tokio::spawn(async move {
            let _child = child;
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(l)) = lines.next_line().await {
                let Ok(v) = serde_json::from_str::<Value>(&l) else {
                    continue;
                };
                // A request from the extension: run the host operation, answer with the same id.
                if v["method"] == "host" && !v["id"].is_null() {
                    let (host, tx, id) = (host2.clone(), tx2.clone(), v["id"].clone());
                    let (op, args) = (
                        v["params"]["op"].as_str().unwrap_or_default().to_string(),
                        v["params"]["args"].clone(),
                    );
                    tokio::spawn(async move {
                        let reply = match host.call(op, args).await {
                            Ok(r) => json!({ "jsonrpc": "2.0", "id": id, "result": r }),
                            Err(e) => json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32000, "message": e } }),
                        };
                        let _ = tx.send(reply.to_string());
                    });
                    continue;
                }
                if let Some(id) = v["id"].as_u64() {
                    if let Some(done) = p2.lock().unwrap().remove(&id) {
                        let r = match v.get("error") {
                            Some(e) => Err(e["message"].as_str().unwrap_or("error").to_string()),
                            None => Ok(v["result"].clone()),
                        };
                        let _ = done.send(r);
                    }
                    continue;
                }
                let text = v["params"]["text"].as_str().unwrap_or_default().to_string();
                match v["method"].as_str() {
                    Some("notify") => {
                        if let Some(tx) = sink.events.get() {
                            let _ = tx.send(AgentEvent::Stream(StreamEvent::Notice {
                                text: format!("[{n2}] {text}"),
                            }));
                        }
                    }
                    Some("ui") => {
                        if let Err(e) = sink.ui(&n2, v["params"].clone()) {
                            tracing::warn!(extension = %n2, "{e}");
                        }
                    }
                    Some("remind") => {
                        let wake = v["params"]["wake"] == true;
                        if let Err(e) = sink.remind(&n2, text, wake) {
                            tracing::warn!(extension = %n2, "{e}");
                        }
                    }
                    Some("log") => tracing::info!(extension = %n2, "{text}"),
                    _ => {}
                }
            }
            for (_, done) in p2.lock().unwrap().drain() {
                let _ = done.send(Err("the extension has exited".into()));
            }
        });
        let mut me = ProcessExt {
            name: name.into(),
            timeout: None,
            reg: Registration::default(),
            routes: Vec::new(),
            tx,
            pending,
            ids: AtomicU64::new(1),
            pid,
        };
        let init = me
            .request(
                "initialize",
                json!({
                    "apiVersion": novacode_script::ext::API_VERSION,
                    "cwd": cwd,
                    "session": session,
                    "home": host.home(),
                    "settings": host.settings_of(name),
                }),
                Duration::from_secs(10),
            )
            .await?;
        me.timeout = init["timeoutMs"]
            .as_u64()
            .map(|ms| Duration::from_millis(ms.clamp(1, 60_000)));
        for t in init["tools"].as_array().into_iter().flatten() {
            let n = t["name"].as_str().unwrap_or_default().to_string();
            me.reg.tools.push(ToolReg {
                name: n.clone(),
                description: t["description"].as_str().unwrap_or_default().into(),
                schema: t["parameters"].clone(),
                exposure: t["exposure"].as_str().map(String::from),
                handler: me.routes.len() as u32,
            });
            me.routes.push(("tool", n));
        }
        for c in init["commands"].as_array().into_iter().flatten() {
            let n = c["name"]
                .as_str()
                .unwrap_or_default()
                .trim_start_matches('/')
                .to_string();
            me.reg.commands.push(CommandReg {
                name: n.clone(),
                description: c["description"].as_str().unwrap_or_default().into(),
                handler: me.routes.len() as u32,
            });
            me.routes.push(("command", n));
        }
        for e in init["events"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|e| e.as_str())
        {
            me.reg.events.push((e.to_string(), me.routes.len() as u32));
            me.routes.push(("event", e.to_string()));
        }
        Ok(Arc::new(me))
    }

    async fn request(&self, method: &str, params: Value, timeout: Duration) -> Result<Value, String> {
        let id = self.ids.fetch_add(1, Ordering::Relaxed);
        let (done, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, done);
        let msg = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        if self.tx.send(msg.to_string()).is_err() {
            return Err("the extension has exited".into());
        }
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(r)) => r,
            Ok(Err(_)) => Err("the extension has exited".into()),
            Err(_) => {
                self.pending.lock().unwrap().remove(&id);
                Err(format!("hung for more than {}s", timeout.as_secs()))
            }
        }
    }
}

impl Backend for ProcessExt {
    fn name(&self) -> &str {
        &self.name
    }
    fn hook_timeout(&self) -> Option<Duration> {
        self.timeout
    }
    fn reg(&self) -> &Registration {
        &self.reg
    }
    fn invoke<'a>(
        &'a self,
        handler: u32,
        payload: &'a Value,
        timeout: Duration,
    ) -> BoxFuture<'a, Result<Value, Failure>> {
        Box::pin(async move {
            let Some((kind, name)) = self.routes.get(handler as usize) else {
                return Err(Failure::Threw("unknown handler".into()));
            };
            let params = match *kind {
                "tool" => {
                    json!({ "name": name, "args": payload["args"], "cwd": payload["cwd"], "session": payload["session"] })
                }
                "command" => json!({ "name": name, "args": payload }),
                _ => json!({ "event": name, "payload": payload }),
            };
            self.request(kind, params, timeout).await.map_err(|e| {
                if e.contains("hung") || e.contains("exited") {
                    Failure::Fatal(e)
                } else {
                    Failure::Threw(e)
                }
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXT: &str = r#"
import json, sys
def send(m): print(json.dumps(m), flush=True)
for line in sys.stdin:
    m = json.loads(line); p = m.get("params", {})
    if "id" not in m and m.get("method") == "shutdown": break
    if m.get("method") is None: continue  # an answer to our host request, read below
    if m["method"] == "initialize":
        greeting = p["settings"]["greeting"]
        r = {"timeoutMs": 20000, "tools": [{"name": "py_upper", "description": "upper", "parameters": {"type": "object"}}], "commands": [{"name": "/pyhi", "description": "hi"}], "events": ["before_tool"]}
    elif m["method"] == "tool": r = p["args"]["t"].upper()
    elif m["method"] == "command":
        send({"jsonrpc": "2.0", "method": "notify", "params": {"text": "ran"}})
        send({"jsonrpc": "2.0", "id": "h1", "method": "host", "params": {"op": "settings", "args": {"path": "extensions.py.greeting"}}})
        ans = json.loads(sys.stdin.readline())
        r = greeting + " " + p["args"] + " " + ans["result"]
    elif m["method"] == "event": r = {"block": "py says no"} if p["payload"]["tool"] == "bash" else None
    send({"jsonrpc": "2.0", "id": m["id"], "result": r})
"#;

    #[tokio::test]
    async fn speaks_json_rpc() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("e.py"), EXT).unwrap();
        let sink: Arc<crate::extensions::Sink> = Arc::default();
        let (tx, mut rx) = tokio::sync::broadcast::channel(4);
        let _ = sink.events.set(tx);
        let host = Arc::new(crate::extensions::Host::new(
            "py",
            d.path(),
            d.path(),
            sink,
            Arc::new(json!({"extensions": {"py": {"greeting": "hey"}}})),
        ));
        let c = Config {
            command: "python3".into(),
            args: vec![d.path().join("e.py").display().to_string()],
            ..Default::default()
        };
        let e = ProcessExt::start("py", &c, d.path(), "s", &d.path().join("log"), host)
            .await
            .unwrap();
        let t = Duration::from_secs(5);
        assert_eq!(e.reg.tools[0].name, "py_upper");
        assert_eq!(e.invoke(0, &json!({"args": {"t": "ab"}}), t).await, Ok(json!("AB")));
        assert_eq!(e.timeout, Some(Duration::from_secs(20)));
        // `greeting` came in initialize's settings, the last word through a host call.
        assert_eq!(e.invoke(1, &json!("nova"), t).await, Ok(json!("hey nova hey")));
        assert!(
            matches!(rx.recv().await.unwrap(), AgentEvent::Stream(StreamEvent::Notice { text }) if text == "[py] ran")
        );
        assert_eq!(
            e.invoke(2, &json!({"tool": "bash"}), t).await,
            Ok(json!({"block": "py says no"}))
        );
    }
}
