//! Super mode end to end (L10.9 build steps 4-5), with real processes on the scripted
//! model: super spawns a write task in a git repo; the task asks a question (relayed to
//! super's window with its label), edits, commits and reports `done` citing its proof
//! number; code starts its review task, which approves; super merges after the user's OK
//! on the merge card, the task rebases and reports, code fast-forwards main, and super closes
//! it after the close card.
//!
//! Each step that could race another waits for super to go idle: the user's answer to the
//! relayed card and two gate files the scripted commands wait for are given only after
//! super's previous turn ended, so every wake is its own turn and takes its own script line.
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
        .env_remove("NOVACODE_AGENT_DEPTH")
        .env_remove("NOVACODE_AGENT_CHAIN")
        .stdin(std::process::Stdio::null());
    c
}

const GIT: &str = "git -c user.name=t -c user.email=t@t -c commit.gpgsign=false";

fn git(dir: &Path, args: &str) -> String {
    let o = Command::new("sh")
        .arg("-c")
        .arg(format!("{GIT} {args}"))
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(o.status.success(), "git {args}: {o:?}");
    String::from_utf8_lossy(&o.stdout).trim().to_string()
}

/// The super dir's board (one root, so one folder under `<home>/super`).
fn board(home: &Path) -> Value {
    let Some(Ok(dir)) = std::fs::read_dir(home.join("super")).ok().and_then(|mut d| d.next()) else {
        return Value::Null;
    };
    let text = std::fs::read_to_string(dir.path().join("board.json")).unwrap_or("{}".into());
    serde_json::from_str(&text).unwrap_or(Value::Null)
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

/// Ends super and every task session on its board, also when the test fails.
struct Cleanup(PathBuf, String);
impl Drop for Cleanup {
    fn drop(&mut self) {
        let sessions: Vec<String> = board(&self.0)["tasks"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|t| t["session"].as_str().map(String::from))
            .collect();
        for id in sessions.iter().chain([&self.1]) {
            let _ = novacode(&self.0).args(["stop", id]).output();
        }
    }
}

/// A shell loop that waits (at most 60 s) for `gate` to exist.
fn wait_for(gate: &Path) -> String {
    format!(
        "for i in $(seq 1 1200); do [ -f {g} ] && break; sleep 0.05; done; [ -f {g} ]",
        g = gate.display()
    )
}

fn script(dir: &Path, name: &str, lines: &[Value]) {
    let text: String = lines.iter().map(|l| format!("{l}\n")).collect();
    std::fs::write(dir.join(format!("{name}.jsonl")), text).unwrap();
}

fn tool(name: &str, args: Value) -> Value {
    json!({"text": "", "tools": [{"name": name, "args": args}]})
}

fn say(text: &str) -> Value {
    json!({"text": text})
}

/// Super's message to the user (its `say` tool), then the empty reply that ends its turn.
fn tell(text: &str) -> [Value; 2] {
    [tool("say", json!({"kind": "update", "body": text})), json!({})]
}

/// Waits (at most 30 s) until `ok` holds.
fn until(what: &str, ok: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while !ok() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Starts super in the background; its session id.
fn start_super(home: &Path, root: &Path, scripts: &Path) -> String {
    let out = novacode(home)
        .current_dir(root)
        .args(["--super", "-m", "faux/script"])
        .env("NOVACODE_FAUX_SCRIPT_DIR", scripts)
        .output()
        .unwrap();
    let sup = String::from_utf8_lossy(&out.stdout).trim().to_string();
    assert!(out.status.success() && !sup.is_empty(), "{out:?}");
    sup
}

fn run_file(home: &Path, session: &str) -> PathBuf {
    home.join("run").join(format!("{session}.json"))
}

/// L10.9 R10: super restarted without a window finds its task's process gone; the
/// "Resume tasks" card waits until a window connects later, and answering it resumes the task.
#[test]
fn restart_recovery_card_waits_for_a_late_window() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().canonicalize().unwrap();
    let (home, root, scripts) = (base.join("home"), base.join("projects"), base.join("scripts"));
    let app = root.join("app");
    for d in [&home, &app, &scripts] {
        std::fs::create_dir_all(d).unwrap();
    }
    let faux = json!({"model": "faux/script"});
    let settings = json!({
        "memory": {"enabled": false},
        "notifications": {"enabled": false},
        "super": {"tiers": {"quick": faux, "standard": faux, "deep": faux, "review": faux}}
    });
    std::fs::write(home.join("settings.json"), settings.to_string()).unwrap();
    std::fs::write(app.join("README"), "x").unwrap();
    git(&app, "init -q -b main");
    git(&app, "add .");
    git(&app, "commit -qm init");
    let label = "T1-note";
    script(
        &scripts,
        "super",
        &[
            tool(
                "task",
                json!({"action": "spawn", "request": "add note.txt", "folder": "app", "slug": "note",
                       "write": true, "tier": "quick", "criteria": ["note.txt exists"]}),
            ),
            tell("spawned")[0].clone(),
            tell("spawned")[1].clone(),
        ],
    );
    // The task works until it is stopped.
    let never = base.join("never");
    script(&scripts, label, &[tool("bash", json!({"command": wait_for(&never)}))]);

    let sup = start_super(&home, &root, &scripts);
    let mut cleanup = Cleanup(home.clone(), sup.clone());
    let run: Value = serde_json::from_str(&std::fs::read_to_string(run_file(&home, &sup)).unwrap()).unwrap();
    let mut sock = UnixStream::connect(run["socket"].as_str().unwrap()).unwrap();
    sock.write_all(format!("{}\n", json!({"id": "p1", "type": "prompt", "text": "add a note"})).as_bytes())
        .unwrap();
    until("the task runs", || {
        let t = task(&home, label);
        t["state"] == "running" && t["session"].as_str().is_some_and(|s| run_file(&home, s).exists())
    });
    let session = task(&home, label)["session"].as_str().unwrap().to_string();
    drop(sock);

    // Super stops, then its task's process goes.
    novacode(&home).args(["stop", &sup]).output().unwrap();
    until("super gone", || !run_file(&home, &sup).exists());
    novacode(&home).args(["stop", &session]).output().unwrap();
    until("task gone", || !run_file(&home, &session).exists());

    // Resumed, the task proves it ran again.
    let resumed = base.join("resumed");
    script(
        &scripts,
        label,
        &[
            tool("bash", json!({"command": format!("touch {}", resumed.display())})),
            say("resumed"),
        ],
    );
    let sup = start_super(&home, &root, &scripts);
    cleanup.1 = sup.clone();
    until("marked interrupted", || task(&home, label)["state"] == "interrupted");
    // The window connects after recovery ran.
    std::thread::sleep(Duration::from_secs(1));
    let run: Value = serde_json::from_str(&std::fs::read_to_string(run_file(&home, &sup)).unwrap()).unwrap();
    let sock = UnixStream::connect(run["socket"].as_str().unwrap()).unwrap();
    sock.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
    let mut conn = BufReader::new(sock);
    let card = loop {
        let mut l = String::new();
        conn.read_line(&mut l)
            .unwrap_or_else(|e| panic!("{e}: no Resume tasks card; board {}", board(&home)));
        assert!(!l.is_empty(), "super closed the connection");
        let m: Value = serde_json::from_str(&l).unwrap();
        if m["type"] == "ask" && m["questions"][0]["header"] == "Resume tasks" {
            break m;
        }
    };
    let q = &card["questions"][0];
    assert_eq!(q["options"][0]["label"], label, "{card}");
    let reply =
        json!({"type": "answer", "ask": card["id"], "replies": [{"question": q["question"], "chosen": [label]}]});
    conn.get_mut().write_all(format!("{reply}\n").as_bytes()).unwrap();
    until("the task resumed", || resumed.exists());
}

#[test]
fn super_runs_a_write_task_through_review_merge_and_close() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().canonicalize().unwrap();
    let (home, root, scripts) = (base.join("home"), base.join("projects"), base.join("scripts"));
    let app = root.join("app");
    for d in [&home, &app, &scripts] {
        std::fs::create_dir_all(d).unwrap();
    }
    // No helper model calls (they would use up script lines), no desktop notifications;
    // every tier on the scripted model.
    let faux = json!({"model": "faux/script"});
    let settings = json!({
        "memory": {"enabled": false},
        "notifications": {"enabled": false},
        "super": {"tiers": {"quick": faux, "standard": faux, "deep": faux, "review": faux}}
    });
    std::fs::write(home.join("settings.json"), settings.to_string()).unwrap();
    std::fs::write(app.join("README"), "x").unwrap();
    git(&app, "init -q -b main");
    git(&app, "add .");
    git(&app, "commit -qm init");
    let (review_gate, merge_gate) = (base.join("review-gate"), base.join("merge-gate"));

    let (label, review) = ("T1-note", "T2-review-T1-note");
    script(
        &scripts,
        "super",
        &[tool(
            "task",
            json!({"action": "spawn", "request": "add note.txt", "folder": "app", "slug": "note",
                       "write": true, "tier": "quick", "criteria": ["note.txt exists on main"]}),
        )]
        .into_iter()
        .chain(tell("spawned"))
        // Wake: T1 reported done, its review starts.
        .chain(tell("waiting for the review"))
        // Wake: the review approved it.
        .chain([tool("task", json!({"action": "merge", "task": label}))])
        .chain(tell("merging"))
        // Wake: T1 merged; close it.
        .chain([tool("task", json!({"action": "close", "task": label}))])
        .chain(tell("closed"))
        .collect::<Vec<_>>(),
    );
    let ask = json!({"questions": [{"question": "Which file name?", "options": [{"label": "note.txt"}, {"label": "other"}]}]});
    script(
        &scripts,
        label,
        &[
            tool("ask_user", ask),
            tool("write", json!({"path": "note.txt", "content": "hello\n"})),
            tool(
                "bash",
                json!({"command": format!("{GIT} add -A && {GIT} commit -qm note && test -f note.txt")}),
            ),
            tool(
                "report",
                json!({"state": "done", "summary": "note.txt added and committed", "checks": ["#1"]}),
            ),
            say("reported"),
            // The merge prompt: the task rebases in its worktree; code fast-forwards main (R29).
            tool(
                "bash",
                json!({"command": format!("{} && {GIT} rebase -q main && test -f note.txt", wait_for(&merge_gate))}),
            ),
            tool("report", json!({"merge": "rebased", "summary": "rebased on main"})),
            say("rebased"),
        ],
    );
    script(
        &scripts,
        review,
        &[
            tool(
                "bash",
                json!({"command": format!("{} && test -f note.txt", wait_for(&review_gate))}),
            ),
            tool(
                "report",
                json!({"verdict": "approved", "findings": ["Criterion 1: met, note.txt exists (#1)"], "checks": ["#1"]}),
            ),
            say("approved"),
        ],
    );

    let out = novacode(&home)
        .current_dir(&root)
        .args(["--super", "-m", "faux/script"])
        .env("NOVACODE_FAUX_SCRIPT_DIR", &scripts)
        .output()
        .unwrap();
    let sup = String::from_utf8_lossy(&out.stdout).trim().to_string();
    assert!(out.status.success() && !sup.is_empty(), "{out:?}");
    let _cleanup = Cleanup(home.clone(), sup.clone());

    let run: Value =
        serde_json::from_str(&std::fs::read_to_string(home.join("run").join(format!("{sup}.json"))).unwrap()).unwrap();
    let sock = UnixStream::connect(run["socket"].as_str().unwrap()).unwrap();
    sock.set_read_timeout(Some(Duration::from_secs(60))).unwrap();
    let mut conn = BufReader::new(sock);
    let send = |conn: &mut BufReader<UnixStream>, v: Value| {
        conn.get_mut().write_all(format!("{v}\n").as_bytes()).unwrap();
    };
    let answer = |conn: &mut BufReader<UnixStream>, m: &Value, pick: &str| {
        let q = &m["questions"][0];
        send(
            conn,
            json!({"type": "answer", "ask": m["id"], "replies": [{"question": q["question"], "chosen": [pick]}]}),
        );
    };
    send(
        &mut conn,
        json!({"id": "p1", "type": "prompt", "text": "add a note to app"}),
    );

    // Super's turns, in order: the spawn, the wake on T1's done, the wake on the
    // approval (merge), the wake on the merge (close).
    let mut ends = 0;
    let mut relayed: Option<Value> = None;
    let mut cards = Vec::new();
    let mut said = String::new();
    let deadline = Instant::now() + Duration::from_secs(120);
    while ends < 4 {
        assert!(
            Instant::now() < deadline,
            "stuck after {ends} turns; cards {cards:?}; board {}",
            board(&home)
        );
        let mut l = String::new();
        conn.read_line(&mut l)
            .unwrap_or_else(|e| panic!("{e} after {ends} turns; board {}", board(&home)));
        assert!(!l.is_empty(), "super closed the connection");
        let m: Value = serde_json::from_str(&l).unwrap();
        match m["type"].as_str() {
            Some("ask") => {
                let q = &m["questions"][0];
                cards.push(format!("{} | {}", q["header"], q["question"]));
                match q["header"].as_str() {
                    Some(h) if h == format!("{label}: merge") => answer(&mut conn, &m, "Merge"),
                    Some(h) if h == format!("{label}: close") => answer(&mut conn, &m, "Close"),
                    _ if ends >= 1 => answer(&mut conn, &m, "note.txt"),
                    // The task's own question: answered once super's spawn turn ended.
                    _ => relayed = Some(m.clone()),
                }
            }
            Some("approve") => panic!("unexpected approval card: {m}"),
            Some("event") if m["event"]["type"] == "message" => said.push_str(&m["event"].to_string()),
            Some("event") if m["event"]["type"] == "agent_end" => {
                ends += 1;
                match ends {
                    1 => {
                        if let Some(r) = relayed.take() {
                            answer(&mut conn, &r, "note.txt");
                        }
                    }
                    2 => std::fs::write(&review_gate, "").unwrap(),
                    3 => std::fs::write(&merge_gate, "").unwrap(),
                    _ => {}
                }
            }
            _ => {}
        }
    }

    // The task's question reached super's window, labelled with the task.
    assert!(
        cards.iter().any(|c| c.contains(&format!("[{label}] Which file name?"))),
        "{cards:?}"
    );
    assert!(said.contains(&format!("{label} is merging")), "{said}");
    assert!(said.contains(&format!("Closed {label}")), "{said}");
    // Super's messages are framed by code: the trigger of each run and the task's status.
    assert!(said.contains("\"trigger\":\"reply to you\""), "{said}");
    assert!(
        said.contains(&format!("\"trigger\":\"after {label} reported done\"")),
        "{said}"
    );
    let t = task(&home, label);
    assert_eq!(t["state"], "archived", "{t}");
    let rv = task(&home, review);
    assert_eq!(rv["state"], "archived", "{rv}");
    assert_eq!(rv["reviewer_of"], label, "{rv}");
    // Merged into main in the main checkout; the merged worktree is gone.
    assert_eq!(std::fs::read_to_string(app.join("note.txt")).unwrap(), "hello\n");
    assert_eq!(git(&app, "branch --show-current"), "main");
    assert!(!app.join(".worktrees").join(label).exists(), "worktree removed");
}
