//! `novacode --acp` (L8.12) end to end: an editor's JSON-RPC over stdio, a real
//! `--rpc` session child, and the scripted `faux/script` model.

use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{Receiver, channel};
use std::time::Duration;

/// 1×1 PNG.
const PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==";

fn start(cwd: &std::path::Path, home: &std::path::Path, script: &std::path::Path) -> Editor {
    let mut child = Command::new(env!("CARGO_BIN_EXE_novacode"))
        .arg("--acp")
        .current_dir(cwd)
        .env("NOVACODE_HOME", home)
        .env("NOVACODE_FAUX_SCRIPT", script)
        .env_remove("NOVACODE_FAUX_SCRIPT_DIR")
        .env_remove("NOVACODE_TASK")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();
    let (tx, rx) = channel();
    std::thread::spawn(move || {
        for l in BufReader::new(stdout).lines().map_while(Result::ok) {
            let v: Value = serde_json::from_str(&l).expect("every line is JSON");
            if tx.send(v).is_err() {
                break;
            }
        }
    });
    let mut ed = Editor {
        child,
        stdin,
        rx,
        seen: vec![],
    };
    ed.send(json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
        "protocolVersion": 1,
        "clientCapabilities": {"fs": {"readTextFile": true, "writeTextFile": true}, "terminal": true,
            "session": {"notices": {}}, "elicitation": {"form": {}}}
    }}));
    let init = ed.until(1, "once");
    assert_eq!(init["result"]["protocolVersion"], 1);
    assert_eq!(init["result"]["agentCapabilities"]["promptCapabilities"]["image"], true);
    assert_eq!(init["result"]["agentCapabilities"]["loadSession"], true);
    ed
}

/// A home whose default model is `faux/script`, a work folder, and the script.
fn setup(dir: &std::path::Path, script: &str) -> (std::path::PathBuf, std::path::PathBuf, std::path::PathBuf) {
    let (home, cwd) = (dir.join("home"), dir.join("work"));
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&cwd).unwrap();
    std::fs::write(home.join("settings.json"), r#"{"model": "faux/script"}"#).unwrap();
    let path = dir.join("script.jsonl");
    std::fs::write(&path, script).unwrap();
    (home, cwd, path)
}

struct Editor {
    child: Child,
    stdin: ChildStdin,
    rx: Receiver<Value>,
    /// Every message the agent sent, in order.
    seen: Vec<Value>,
}

impl Editor {
    fn send(&mut self, v: Value) {
        writeln!(self.stdin, "{v}").unwrap();
        self.stdin.flush().unwrap();
    }

    /// Read until the response to request `id`, answering permission requests with
    /// `allow` on the way.
    fn until(&mut self, id: u64, allow: &str) -> Value {
        loop {
            let m = self
                .rx
                .recv_timeout(Duration::from_secs(60))
                .unwrap_or_else(|_| panic!("no response to {id}; saw {:#?}", self.seen));
            self.seen.push(m.clone());
            if m["method"] == "session/request_permission" {
                let rid = m["id"].clone();
                self.send(json!({"jsonrpc": "2.0", "id": rid, "result": {"outcome": {"outcome": "selected", "optionId": allow}}}));
            } else if m["method"] == "elicitation/create" {
                let rid = m["id"].clone();
                self.send(json!({"jsonrpc": "2.0", "id": rid, "result": {"action": "accept", "content": {"q00": "Rust", "q00_text": "with tests"}}}));
            } else if m["id"] == id && m.get("method").is_none() {
                return m;
            }
        }
    }

    fn wait_for_request(&mut self, method: &str) -> Value {
        loop {
            let m = self.rx.recv_timeout(Duration::from_secs(15)).expect("a client request");
            self.seen.push(m.clone());
            if m["method"] == method {
                return m;
            }
        }
    }

    fn updates(&self, kind: &str) -> Vec<&Value> {
        self.seen
            .iter()
            .filter(|m| m["method"] == "session/update" && m["params"]["update"]["sessionUpdate"] == kind)
            .map(|m| &m["params"]["update"])
            .collect()
    }
}

#[test]
fn a_prompt_with_an_image_runs_a_tool_and_shows_its_diff() {
    let dir = tempfile::tempdir().unwrap();
    let script = concat!(
        r#"{"text": "writing", "tools": [{"name": "write", "args": {"path": "a.txt", "content": "hello acp\n"}}]}"#,
        "\n",
        r#"{"text": "done writing"}"#,
        "\n"
    );
    let (home, cwd, script) = setup(dir.path(), script);
    let mut ed = start(&cwd, &home, &script);

    ed.send(json!({"jsonrpc": "2.0", "id": 2, "method": "session/new", "params": {"cwd": cwd, "mcpServers": []}}));
    let new = ed.until(2, "once");
    let sid = new["result"]["sessionId"].as_str().expect("a session id").to_string();
    let opts = new["result"]["configOptions"].as_array().expect("config options");
    let model = opts.iter().find(|o| o["id"] == "model").expect("a model option");
    assert_eq!(model["currentValue"], "faux/script");
    assert!(opts.iter().any(|o| o["id"] == "thought_level"));
    assert!(opts.iter().any(|o| o["id"] == "mode"), "plan mode is offered: {opts:?}");

    ed.send(
        json!({"jsonrpc": "2.0", "id": 3, "method": "session/prompt", "params": {"sessionId": sid, "prompt": [
            {"type": "text", "text": "write the file, see"},
            {"type": "image", "mimeType": "image/png", "data": PNG}
        ]}}),
    );
    let done = ed.until(3, "once");
    assert_eq!(done["result"]["stopReason"], "end_turn", "{done}");
    assert_eq!(std::fs::read_to_string(cwd.join("a.txt")).unwrap(), "hello acp\n");

    let calls = ed.updates("tool_call");
    let write = calls
        .iter()
        .find(|c| c["kind"] == "edit")
        .expect("the write as a tool call");
    assert_eq!(write["content"][0]["type"], "diff");
    assert_eq!(write["content"][0]["newText"], "hello acp\n");
    assert!(write["locations"][0]["path"].as_str().unwrap().ends_with("a.txt"));
    let id = write["toolCallId"].clone();
    assert!(
        ed.updates("tool_call_update")
            .iter()
            .any(|u| u["toolCallId"] == id && u["status"] == "completed"),
        "the write completed"
    );
    let text: String = ed
        .updates("agent_message_chunk")
        .iter()
        .filter_map(|u| u["content"]["text"].as_str())
        .collect();
    assert!(text.contains("done writing"), "reply text: {text}");
    let notices: Vec<&str> = ed
        .updates("notice")
        .iter()
        .filter_map(|u| u["title"].as_str())
        .collect();
    assert!(
        notices.iter().any(|n| n.contains("attached")),
        "the pasted image was attached: {notices:?}"
    );

    // A slash command handled by the session ends the turn at once.
    ed.send(
        json!({"jsonrpc": "2.0", "id": 4, "method": "session/prompt", "params": {"sessionId": sid, "prompt": [
            {"type": "text", "text": "/mode"}
        ]}}),
    );
    assert_eq!(ed.until(4, "once")["result"]["stopReason"], "end_turn");

    ed.send(
        json!({"jsonrpc": "2.0", "id": 5, "method": "session/set_config_option", "params": {
        "sessionId": sid, "configId": "mode", "value": "plan"}}),
    );
    let set = ed.until(5, "once");
    let mode = set["result"]["configOptions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|o| o["id"] == "mode")
        .cloned();
    assert_eq!(mode.unwrap()["currentValue"], "plan", "{set}");

    ed.send(json!({"jsonrpc": "2.0", "id": 6, "method": "session/list", "params": {"cwd": cwd}}));
    let list = ed.until(6, "once");
    assert!(
        list["result"]["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["sessionId"] == sid),
        "{list}"
    );

    ed.send(json!({"jsonrpc": "2.0", "id": 7, "method": "session/close", "params": {"sessionId": sid}}));
    ed.until(7, "once");

    // Reopening it from the editor's history replays what was said and done.
    ed.seen.clear();
    ed.send(json!({"jsonrpc": "2.0", "id": 8, "method": "session/load", "params": {
        "sessionId": sid, "cwd": cwd, "mcpServers": []}}));
    let load = ed.until(8, "once");
    assert!(load.get("error").is_none(), "{load}");
    let user: String = ed
        .updates("user_message_chunk")
        .iter()
        .filter_map(|u| u["content"]["text"].as_str())
        .collect();
    assert!(user.contains("write the file"), "replayed prompt: {user}");
    assert!(
        ed.updates("tool_call")
            .iter()
            .any(|c| c["content"][0]["type"] == "diff")
    );
    drop(ed.stdin);
    let status = ed.child.wait().unwrap();
    assert!(status.success());
}

fn wait_for_exit(child: &mut Child) {
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(status.success(), "ACP exited with {status}");
            return;
        }
        if std::time::Instant::now() >= deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("ACP did not exit within 15 seconds");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn same_tool_approvals_never_borrow_another_calls_diff() {
    let dir = tempfile::tempdir().unwrap();
    let (home, cwd, script) = setup(
        dir.path(),
        concat!(
            r#"{"tools": [{"name": "write", "args": {"path": "a.txt", "content": "first"}}, {"name": "write", "args": {"path": "b.txt", "content": "second"}}]}"#,
            "\n",
            r#"{"text": "done"}"#,
            "\n"
        ),
    );
    std::fs::write(
        home.join("settings.json"),
        r#"{"model":"faux/script","permissions":{"mode":"ask"}}"#,
    )
    .unwrap();
    let mut ed = start(&cwd, &home, &script);
    ed.send(json!({"jsonrpc":"2.0","id":2,"method":"session/new","params":{"cwd":cwd,"mcpServers":[]}}));
    let sid = ed.until(2, "once")["result"]["sessionId"].clone();
    ed.send(json!({"jsonrpc":"2.0","id":3,"method":"session/prompt","params":{"sessionId":sid,"prompt":[{"type":"text","text":"write both"}]}}));
    assert_eq!(ed.until(3, "once")["result"]["stopReason"], "end_turn");
    let cards: Vec<_> = ed
        .seen
        .iter()
        .filter(|m| m["method"] == "session/request_permission")
        .map(|m| &m["params"]["toolCall"])
        .collect();
    assert_eq!(cards.len(), 2, "{cards:?}");
    assert_ne!(cards[0]["toolCallId"], cards[1]["toolCallId"]);
    for (card, path) in cards.iter().zip(["a.txt", "b.txt"]) {
        assert!(card["toolCallId"].as_str().unwrap().starts_with("approval-"));
        let content = card["content"].as_array().unwrap();
        assert!(content.iter().all(|c| c["type"] != "diff"), "{card}");
        let text = content[0]["content"]["text"].as_str().unwrap();
        assert!(text.contains("write:") && text.contains(path), "{card}");
        assert!(text.contains("ask mode"), "the exact approval reason is shown: {card}");
    }
    assert_eq!(std::fs::read_to_string(cwd.join("a.txt")).unwrap(), "first");
    assert_eq!(std::fs::read_to_string(cwd.join("b.txt")).unwrap(), "second");
    drop(ed.stdin);
    wait_for_exit(&mut ed.child);
}

#[test]
fn unanswered_permissions_and_forms_cancel_on_close_and_eof() {
    for (tool, method) in [
        (
            json!({"name":"write","args":{"path":"a.txt","content":"hello"}}),
            "session/request_permission",
        ),
        (
            json!({"name":"ask_user","args":{"questions":[{"question":"Name?"}]}}),
            "elicitation/create",
        ),
    ] {
        for close in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let (home, cwd, script) = setup(dir.path(), &format!("{}\n", json!({"tools":[tool]})));
            std::fs::write(
                home.join("settings.json"),
                r#"{"model":"faux/script","permissions":{"mode":"ask"}}"#,
            )
            .unwrap();
            let mut ed = start(&cwd, &home, &script);
            ed.send(json!({"jsonrpc":"2.0","id":2,"method":"session/new","params":{"cwd":cwd,"mcpServers":[]}}));
            let sid = ed.until(2, "once")["result"]["sessionId"].clone();
            ed.send(json!({"jsonrpc":"2.0","id":3,"method":"session/prompt","params":{"sessionId":sid,"prompt":[{"type":"text","text":"start"}]}}));
            ed.wait_for_request(method);
            if close {
                ed.send(json!({"jsonrpc":"2.0","id":4,"method":"session/close","params":{"sessionId":sid}}));
                let response = ed.until(4, "deny");
                assert_eq!(response["result"], json!({}), "{response}");
            }
            drop(ed.stdin);
            wait_for_exit(&mut ed.child);
        }
    }
}

#[test]
fn config_changes_report_applied_values_and_reject_invalid_values() {
    let dir = tempfile::tempdir().unwrap();
    let (home, cwd, script) = setup(dir.path(), "{\"text\":\"hello\"}\n");
    let mut ed = start(&cwd, &home, &script);
    ed.send(json!({"jsonrpc":"2.0","id":2,"method":"session/new","params":{"cwd":cwd,"mcpServers":[]}}));
    let sid = ed.until(2, "once")["result"]["sessionId"].clone();
    for (id, config, value, expected) in [
        (3, "model", "faux/missing", None),
        (4, "mode", "missing", None),
        (5, "model", "faux/script:low", Some("faux/script")),
        (6, "thought_level", "high", Some("high")),
        (7, "mode", "off", Some("off")),
        (8, "mode", "", None),
        (9, "mode", "plan extra", None),
    ] {
        ed.send(json!({"jsonrpc":"2.0","id":id,"method":"session/set_config_option","params":{"sessionId":sid,"configId":config,"value":value}}));
        let response = ed.until(id, "once");
        if let Some(expected) = expected {
            let options = response["result"]["configOptions"]
                .as_array()
                .unwrap_or_else(|| panic!("{response}"));
            assert_eq!(
                options.iter().find(|o| o["id"] == config).unwrap()["currentValue"],
                expected,
                "{response}"
            );
            if id == 5 {
                assert_eq!(
                    options.iter().find(|o| o["id"] == "thought_level").unwrap()["currentValue"],
                    "low",
                    "model switch applies its thinking suffix"
                );
            }
        } else {
            assert!(
                response.get("error").is_some(),
                "invalid {config} was accepted: {response}"
            );
        }
    }
    drop(ed.stdin);
    wait_for_exit(&mut ed.child);
}

#[test]
fn ask_user_is_an_elicitation_form() {
    let dir = tempfile::tempdir().unwrap();
    let script = concat!(
        r#"{"tools": [{"name": "ask_user", "args": {"questions": [{"question": "Which language?", "options": [{"label": "Rust", "recommended": true}, {"label": "Go"}]}]}}]}"#,
        "\n",
        r#"{"text": "noted"}"#,
        "\n"
    );
    let (home, cwd, script) = setup(dir.path(), script);
    let mut ed = start(&cwd, &home, &script);
    ed.send(json!({"jsonrpc": "2.0", "id": 2, "method": "session/new", "params": {"cwd": cwd, "mcpServers": []}}));
    let sid = ed.until(2, "once")["result"]["sessionId"].clone();
    ed.send(
        json!({"jsonrpc": "2.0", "id": 3, "method": "session/prompt", "params": {"sessionId": sid, "prompt": [
            {"type": "text", "text": "ask me"}
        ]}}),
    );
    assert_eq!(ed.until(3, "once")["result"]["stopReason"], "end_turn");
    let form = ed
        .seen
        .iter()
        .find(|m| m["method"] == "elicitation/create")
        .expect("a form was asked");
    assert_eq!(form["params"]["mode"], "form", "{form}");
    let q = &form["params"]["requestedSchema"]["properties"]["q00"];
    assert_eq!(q["oneOf"][0]["const"], "Rust", "{form}");
    let out: String = ed
        .updates("tool_call_update")
        .iter()
        .filter_map(|u| u["content"][0]["content"]["text"].as_str())
        .collect();
    assert!(
        out.contains("Rust") && out.contains("with tests"),
        "the answer reached the tool: {out}"
    );
    drop(ed.stdin);
    assert!(ed.child.wait().unwrap().success());
}
