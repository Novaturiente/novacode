//! Super's `task` tool end to end (L10.9 "Tasks", "Controls"): a scripted super spawns a
//! real task process in an untrusted repo, which starts only after the trust card is
//! answered, gets its worktree, environment, brief and the project's MCP servers; then
//! super stops it.
#![cfg(target_os = "linux")]

use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
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
        // Run from inside a novacode session, the tests would be its subagents.
        .env_remove("NOVACODE_AGENT_DEPTH")
        .env_remove("NOVACODE_AGENT_CHAIN")
        .stdin(std::process::Stdio::null());
    c
}

fn git(dir: &Path, args: &[&str]) {
    let ok = Command::new("git")
        .args([
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap()
        .status
        .success();
    assert!(ok, "git {args:?}");
}

fn run_file(home: &Path, id: &str) -> Value {
    serde_json::from_str(&std::fs::read_to_string(home.join("run").join(format!("{id}.json"))).unwrap()).unwrap()
}

struct Conn(BufReader<UnixStream>);

impl Conn {
    fn open(home: &Path, id: &str) -> Conn {
        let sock = UnixStream::connect(run_file(home, id)["socket"].as_str().unwrap()).unwrap();
        sock.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
        Conn(BufReader::new(sock))
    }
    fn send(&mut self, v: Value) {
        let mut l = v.to_string();
        l.push('\n');
        self.0.get_mut().write_all(l.as_bytes()).unwrap();
    }
    fn next(&mut self) -> Value {
        let mut l = String::new();
        self.0.read_line(&mut l).expect("no message within 30 s");
        assert!(!l.is_empty(), "the session closed the connection");
        serde_json::from_str(&l).unwrap()
    }
}

/// The super dir's board (one root, so one folder under `<home>/super`).
fn board(home: &Path) -> Value {
    let dir = std::fs::read_dir(home.join("super"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    serde_json::from_str(&std::fs::read_to_string(dir.join("board.json")).unwrap_or("{}".into())).unwrap()
}

fn task(home: &Path, label: &str) -> Value {
    board(home)["tasks"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|t| t["label"] == label)
        .cloned()
        .unwrap_or(Value::Null)
}

struct Stop(PathBuf, Vec<String>);
impl Drop for Stop {
    fn drop(&mut self) {
        for id in &self.1 {
            let _ = novacode(&self.0).args(["stop", id]).output();
        }
    }
}

#[test]
fn super_spawns_a_task_after_the_trust_card_and_stops_it() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().canonicalize().unwrap();
    let (home, root, scripts) = (base.join("home"), base.join("projects"), base.join("scripts"));
    let app = root.join("app");
    for d in [&home, &app, &scripts] {
        std::fs::create_dir_all(d).unwrap();
    }
    // No helper model calls: they would use up script lines.
    std::fs::write(home.join("settings.json"), r#"{"memory": {"enabled": false}}"#).unwrap();
    std::fs::write(
        app.join(".mcp.json"),
        r#"{"mcpServers": {"projsrv": {"command": "true"}}}"#,
    )
    .unwrap();
    std::fs::write(app.join("README"), "x").unwrap();
    git(&app, &["init", "-q", "-b", "main"]);
    git(&app, &["add", "."]);
    git(&app, &["commit", "-qm", "init"]);
    // Untracked and gitignored-like: copied into the worktree by worktree.copy.
    std::fs::write(app.join("AGENTS.md"), "PROJECT-RULE").unwrap();
    let spawn = json!({"name": "task", "args": {
        "action": "spawn", "request": "add a note", "folder": "app", "slug": "note",
        "write": true, "model": "faux/script", "criteria": ["note exists"]
    }});
    let stop = json!({"name": "task", "args": {"action": "stop", "task": "T1-note"}});
    let lines = [
        json!({"text": "spawning", "tools": [spawn]}),
        json!({"text": "spawned"}),
        json!({"text": "stopping", "tools": [stop]}),
        json!({"text": "stopped"}),
    ];
    let script: String = lines.iter().map(|l| format!("{l}\n")).collect();
    std::fs::write(scripts.join("super.jsonl"), script).unwrap();
    std::fs::write(scripts.join("T1-note.jsonl"), "").unwrap();

    let out = novacode(&home)
        .current_dir(&root)
        .args(["--super", "-m", "faux/script"])
        .env("NOVACODE_FAUX_SCRIPT_DIR", &scripts)
        .output()
        .unwrap();
    let sup = String::from_utf8_lossy(&out.stdout).trim().to_string();
    assert!(out.status.success() && !sup.is_empty(), "{out:?}");
    let mut cleanup = Stop(home.clone(), vec![sup.clone()]);

    let mut c = Conn::open(&home, &sup);
    c.send(json!({"id": "p1", "type": "prompt", "text": "go"}));
    let mut asked = false;
    let mut results = String::new();
    loop {
        let m = c.next();
        match m["type"].as_str() {
            Some("ask") => {
                // The task hasn't started: no session on the board, no worktree.
                assert!(task(&home, "T1-note").is_null(), "no row before the card");
                assert!(!app.join(".worktrees").exists());
                let q = &m["questions"][0];
                assert!(q["question"].as_str().unwrap().contains("branches from main"), "{q}");
                c.send(json!({"type": "answer", "ask": m["id"], "replies": [
                    {"question": q["question"], "chosen": ["Trust for this task only"]}
                ]}));
                asked = true;
            }
            Some("approve") => panic!("unexpected approval card: {m}"),
            Some("event") if m["event"]["type"] == "message" => results.push_str(&m["event"].to_string()),
            Some("event") if m["event"]["type"] == "agent_end" => break,
            _ => {}
        }
    }
    assert!(asked, "the trust card was shown");
    assert!(results.contains("Started T1-note"), "{results}");

    let t = task(&home, "T1-note");
    assert_eq!(t["state"], "running", "{t}");
    assert_eq!(t["trusted"], true);
    let session = t["session"].as_str().unwrap().to_string();
    cleanup.1.push(session.clone());
    let wt = app.join(".worktrees/T1-note");
    assert_eq!(t["worktree"], wt.to_str().unwrap());
    assert!(wt.join("AGENTS.md").exists());

    // The task process: detached, its own env, trust given, its project's MCP server configured.
    let run = run_file(&home, &session);
    let pid = run["pid"].as_u64().unwrap();
    let env = std::fs::read(format!("/proc/{pid}/environ")).unwrap();
    let env: Vec<String> = env
        .split(|b| *b == 0)
        .map(|v| String::from_utf8_lossy(v).into_owned())
        .collect();
    assert!(env.contains(&"NOVACODE_TASK=T1-note".to_string()), "{env:?}");
    assert!(env.contains(&format!("NOVACODE_EDIT_RULE=worktree:{}", wt.display())));
    assert!(!env.iter().any(|v| v.starts_with("NOVACODE_BG_CHILD=")));
    assert_eq!(run["state"]["cwd"], wt.to_str().unwrap());
    let cmdline = std::fs::read(format!("/proc/{pid}/cmdline")).unwrap();
    assert!(String::from_utf8_lossy(&cmdline).contains("--trust"));
    let end = Instant::now() + Duration::from_secs(20);
    loop {
        let mcp = run_file(&home, &session)["state"]["mcp"].clone();
        if mcp[1] == 1 {
            break;
        }
        assert!(Instant::now() < end, "the project's MCP server never showed: {mcp}");
        std::thread::sleep(Duration::from_millis(100));
    }
    // It got the brief as its first prompt, and the preamble in its system prompt.
    let mut tc = Conn::open(&home, &session);
    tc.send(json!({"id": "m", "type": "get_messages"}));
    let msgs = loop {
        let m = tc.next();
        if m["type"] == "response" && m["id"] == "m" {
            break m["data"]["messages"].to_string();
        }
    };
    assert!(msgs.contains("add a note") && msgs.contains("1. note exists"), "{msgs}");
    assert!(msgs.contains("You are task T1-note"), "{msgs}");

    c.send(json!({"id": "p2", "type": "prompt", "text": "stop it"}));
    loop {
        let m = c.next();
        if m["type"] == "event" && m["event"]["type"] == "agent_end" {
            break;
        }
    }
    assert_eq!(task(&home, "T1-note")["state"], "stopped");
    assert!(
        novacode(&home)
            .arg("agents")
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).contains(&session[..13]))
            .unwrap(),
        "stop keeps the task's session alive"
    );
}
