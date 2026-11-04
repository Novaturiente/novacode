//! Super's context stays small (L10.9 "The super agent", spec goal: under 50k tokens
//! after 20 task reports): a real super process on a scripted model applies 20 reports
//! from its inbox, each waking it, and its context is measured afterwards.
#![cfg(target_os = "linux")]

use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

fn novacode(home: &Path) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_novacode"));
    c.env("NOVACODE_HOME", home)
        .env_remove("NOVACODE_BG_CHILD")
        .env_remove("NOVACODE_FAUX_SCRIPT")
        .env_remove("NOVACODE_TASK")
        .env_remove("NOVACODE_TASK_TOKEN")
        .env_remove("NOVACODE_SUPER_DIR")
        .env_remove("NOVACODE_EDIT_RULE")
        .env_remove("NOVACODE_AGENT_DEPTH")
        .env_remove("NOVACODE_AGENT_CHAIN")
        .stdin(std::process::Stdio::null());
    c
}

struct Stop(std::path::PathBuf, String);
impl Drop for Stop {
    fn drop(&mut self) {
        let _ = novacode(&self.0).args(["stop", &self.1]).output();
    }
}

/// One request on the session's socket; its response's data.
fn ask(home: &Path, id: &str, cmd: Value) -> Value {
    let run: Value =
        serde_json::from_str(&std::fs::read_to_string(home.join("run").join(format!("{id}.json"))).unwrap()).unwrap();
    let sock = UnixStream::connect(run["socket"].as_str().unwrap()).unwrap();
    sock.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
    let mut r = BufReader::new(sock);
    let mut l = cmd.to_string();
    l.push('\n');
    r.get_mut().write_all(l.as_bytes()).unwrap();
    loop {
        let mut l = String::new();
        r.read_line(&mut l).expect("no answer within 30 s");
        assert!(!l.is_empty(), "the session closed the connection");
        let m: Value = serde_json::from_str(&l).unwrap();
        if m["type"] == "response" && m["id"] == cmd["id"] {
            return m["data"].clone();
        }
    }
}

#[test]
fn super_context_stays_under_50k_tokens_after_20_reports() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().canonicalize().unwrap();
    let (home, root, scripts) = (base.join("home"), base.join("projects"), base.join("scripts"));
    for d in [&home, &root, &scripts] {
        std::fs::create_dir_all(d).unwrap();
    }
    std::fs::write(home.join("settings.json"), r#"{"memory": {"enabled": false}}"#).unwrap();
    // Super's model has no script lines: every wake gets an empty reply.
    std::fs::write(scripts.join("super.jsonl"), "").unwrap();
    // The board a restarted super finds: 20 read tasks a code-made report blocked (no
    // process: a `running` row without a session is started again, audit r5 A13); their
    // own `done` still applies (R18).
    let sup_dir = home.join("super").join(novacode_core::trust::path_key(&root));
    std::fs::create_dir_all(sup_dir.join("inbox")).unwrap();
    let tasks: Vec<Value> = (1..=20)
        .map(|n| {
            json!({"id": format!("T{n}-q"), "label": format!("T{n}-q"), "kind": "work", "type": "question",
                   "state": "blocked", "block": {"reason": "ended its turn without a report", "code_made": true},
                   "token": format!("tok{n}"), "model": "faux/script",
                   "request": "answer it", "criteria": ["an answer"], "started": 1, "updated": 1})
        })
        .collect();
    std::fs::write(sup_dir.join("board.json"), json!({"tasks": tasks}).to_string()).unwrap();

    let out = novacode(&home)
        .current_dir(&root)
        .args(["--super", "-m", "faux/script"])
        .env("NOVACODE_FAUX_SCRIPT_DIR", &scripts)
        .output()
        .unwrap();
    let sup = String::from_utf8_lossy(&out.stdout).trim().to_string();
    assert!(out.status.success() && !sup.is_empty(), "{out:?}");
    let _cleanup = Stop(home.clone(), sup.clone());

    // 20 reports of 70 words each, as tasks write them.
    let words = "the parser reads every token once and keeps a small stack of open scopes; ".repeat(5);
    for n in 1..=20 {
        let r = json!({"id": format!("r{n}"), "task": format!("T{n}-q"), "seq": 1, "token": format!("tok{n}"),
                       "state": "done", "summary": format!("T{n}: {words}"), "checks": []});
        std::fs::write(sup_dir.join("inbox").join(format!("T{n}-q-1.json")), r.to_string()).unwrap();
    }

    let end = Instant::now() + Duration::from_secs(60);
    let state = loop {
        let board: Value = serde_json::from_str(&std::fs::read_to_string(sup_dir.join("board.json")).unwrap()).unwrap();
        let reviewing = board["tasks"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|t| t["state"] == "reviewing")
            .count();
        let s = ask(&home, &sup, json!({"id": "s", "type": "get_state"}));
        // The wakes are hidden notes: in the session file, not in the visible history.
        let file = std::fs::read_to_string(s["file"].as_str().unwrap()).unwrap_or_default();
        let woken = (1..=20).all(|n| file.contains(&format!("T{n}-q reported done")));
        if reviewing == 20 && s["busy"] == false && woken {
            break s;
        }
        assert!(
            Instant::now() < end,
            "reports not all applied: {reviewing} reviewing, {s}"
        );
        std::thread::sleep(Duration::from_millis(200));
    };
    // Faux usage is 0, so the context is the sum of its parts (system prompt, tools, messages).
    let parts = state["host"]["parts"].as_array().unwrap();
    let messages = parts.iter().find(|p| p[0] == "messages").unwrap()[1].as_u64().unwrap();
    let tokens: u64 = parts.iter().map(|p| p[1].as_u64().unwrap()).sum();
    assert!(messages > 20 * 70, "the 20 reports are in the context: {state}");
    assert!(tokens < 50_000, "super's context is {tokens} tokens: {state}");
}
