//! Super mode end to end (L10.9 build steps 4-5), with real processes on the scripted
//! model: super spawns a write task in a git repo; the task asks a question (relayed to
//! super's window with its label), edits, commits and reports `done` citing its proof
//! number; code starts its review task, which approves; super merges after the user's OK
//! on the merge card, the task rebases and reports, code fast-forwards main, and code closes
//! the merged task without a card.
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

/// A row by its label, with or without the super session's `<sid4>-` (L10.9).
fn task(home: &Path, label: &str) -> Value {
    board(home)["tasks"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|t| {
            t["label"] == label
                || t["label"]
                    .as_str()
                    .and_then(|l| l.split_once('-'))
                    .is_some_and(|(_, r)| r == label)
        })
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
    [
        tool(
            "say",
            json!({"kind": "update", "sections": [{"task": "General", "body": text}]}),
        ),
        json!({}),
    ]
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

/// A session's saved file, as text (wakes are saved there, not sent as message events).
fn session_text(home: &Path, session: &str) -> String {
    std::fs::read_dir(home.join("sessions"))
        .into_iter()
        .flatten()
        .flatten()
        .find_map(|d| std::fs::read_to_string(d.path().join(format!("{session}.jsonl"))).ok())
        .unwrap_or_default()
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
    // Named as super shows it, for tmux side panels.
    until("the task session carries its label", || {
        std::fs::read_to_string(run_file(&home, &session))
            .ok()
            .and_then(|t| serde_json::from_str::<Value>(&t).ok())
            .is_some_and(|r| r["state"]["name"] == label)
    });
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

/// S3 (live test): `novacode --super` right after `novacode stop <super>` found the old super
/// still shutting down and attached to it ("the session is over"). It waits for that one to
/// exit and starts a new one.
#[test]
fn relaunch_right_after_stop_starts_a_new_super() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().canonicalize().unwrap();
    let (home, root, scripts) = (base.join("home"), base.join("projects"), base.join("scripts"));
    for d in [&home, &root, &scripts] {
        std::fs::create_dir_all(d).unwrap();
    }
    let settings = json!({"memory": {"enabled": false}, "notifications": {"enabled": false}});
    std::fs::write(home.join("settings.json"), settings.to_string()).unwrap();
    script(&scripts, "super", &tell("hi"));
    let pid = |sup: &str| -> Option<u64> {
        let text = std::fs::read_to_string(run_file(&home, sup)).ok()?;
        serde_json::from_str::<Value>(&text).ok()?["pid"].as_u64()
    };
    let mut sup = start_super(&home, &root, &scripts);
    let mut cleanup = Cleanup(home.clone(), sup.clone());
    let mut old = pid(&sup).unwrap();
    for _ in 0..3 {
        novacode(&home).args(["stop", &sup]).output().unwrap();
        let again = start_super(&home, &root, &scripts);
        cleanup.1 = again.clone();
        let new = pid(&again);
        assert!(
            new.is_some_and(|p| p != old),
            "attached to the super that was shutting down"
        );
        (sup, old) = (again.clone(), new.unwrap());
        let run: Value = serde_json::from_str(&std::fs::read_to_string(run_file(&home, &again)).unwrap()).unwrap();
        let sock = UnixStream::connect(run["socket"].as_str().unwrap()).unwrap();
        sock.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let mut l = String::new();
        BufReader::new(sock).read_line(&mut l).unwrap();
        assert!(l.contains("\"hello\""), "{l}");
    }
}

/// L9 `/kill`: after `novacode stop` the next super resumes the same session; after a
/// `/kill` (its window marks the super folder `fresh`, then stops it) a new session starts,
/// and the board stays.
#[test]
fn super_resumes_after_stop_and_starts_fresh_after_kill() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().canonicalize().unwrap();
    let (home, root, scripts) = (base.join("home"), base.join("projects"), base.join("scripts"));
    for d in [&home, &root, &scripts] {
        std::fs::create_dir_all(d).unwrap();
    }
    let settings = json!({"memory": {"enabled": false}, "notifications": {"enabled": false}});
    std::fs::write(home.join("settings.json"), settings.to_string()).unwrap();
    script(&scripts, "super", &tell("hi"));
    let first = start_super(&home, &root, &scripts);
    let mut cleanup = Cleanup(home.clone(), first.clone());
    let super_dir = std::fs::read_dir(home.join("super"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    until("the board is saved", || super_dir.join("board.json").exists());
    // A session is written with its first message: give super one.
    let run: Value = serde_json::from_str(&std::fs::read_to_string(run_file(&home, &first)).unwrap()).unwrap();
    let mut sock = UnixStream::connect(run["socket"].as_str().unwrap()).unwrap();
    sock.write_all(format!("{}\n", json!({"type": "prompt", "text": "hello"})).as_bytes())
        .unwrap();
    let saved = || {
        std::fs::read_dir(home.join("sessions"))
            .into_iter()
            .flatten()
            .flatten()
            .any(|d| d.path().join(format!("{first}.jsonl")).exists())
    };
    until("super's session is saved", saved);
    drop(sock);
    novacode(&home).args(["stop", &first]).output().unwrap();
    let again = start_super(&home, &root, &scripts);
    assert_eq!(again, first, "a stop resumes the same session");
    std::fs::write(super_dir.join("fresh"), "").unwrap();
    novacode(&home).args(["stop", &again]).output().unwrap();
    let fresh = start_super(&home, &root, &scripts);
    cleanup.1 = fresh.clone();
    assert_ne!(fresh, first, "after /kill a new session starts");
    assert!(super_dir.join("board.json").exists() && !super_dir.join("fresh").exists());
}

/// Live bug (0.8.5): super waiting on its own question card ignored `novacode stop` and
/// SIGTERM; only SIGKILL ended it. Both end it within a few seconds.
#[test]
fn stop_and_sigterm_end_super_waiting_on_a_card() {
    for by_signal in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().canonicalize().unwrap();
        let (home, root, scripts) = (base.join("home"), base.join("projects"), base.join("scripts"));
        for d in [&home, &root, &scripts] {
            std::fs::create_dir_all(d).unwrap();
        }
        let settings = json!({"memory": {"enabled": false}, "notifications": {"enabled": false}});
        std::fs::write(home.join("settings.json"), settings.to_string()).unwrap();
        let ask = json!({"gate": "unclear", "questions": [{"question": "Which one?", "options": [{"label": "a"}, {"label": "b"}]}]});
        script(&scripts, "super", &[tool("ask_user", ask), say("answered")]);
        let sup = start_super(&home, &root, &scripts);
        let _cleanup = Cleanup(home.clone(), sup.clone());
        let run: Value = serde_json::from_str(&std::fs::read_to_string(run_file(&home, &sup)).unwrap()).unwrap();
        let pid = run["pid"].as_u64().unwrap();
        let sock = UnixStream::connect(run["socket"].as_str().unwrap()).unwrap();
        sock.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
        let mut conn = BufReader::new(sock);
        let prompt = json!({"type": "prompt", "text": "pick"});
        conn.get_mut().write_all(format!("{prompt}\n").as_bytes()).unwrap();
        loop {
            let mut l = String::new();
            conn.read_line(&mut l).unwrap();
            assert!(!l.is_empty(), "super closed the connection");
            if serde_json::from_str::<Value>(&l).unwrap()["type"] == "ask" {
                break;
            }
        }
        if by_signal {
            Command::new("kill").args(["-TERM", &pid.to_string()]).status().unwrap();
        } else {
            novacode(&home).args(["stop", &sup]).output().unwrap();
        }
        // Gone: no process, or a zombie not reaped yet.
        let gone = || {
            std::fs::read_to_string(format!("/proc/{pid}/stat")).map_or(true, |s| {
                s.rsplit_once(')').is_some_and(|(_, r)| r.trim_start().starts_with('Z'))
            })
        };
        let deadline = Instant::now() + Duration::from_secs(10);
        while !gone() {
            if Instant::now() > deadline {
                let _ = Command::new("kill").args(["-KILL", &pid.to_string()]).status();
                panic!(
                    "super waiting on a card ignored {}",
                    if by_signal { "SIGTERM" } else { "stop" }
                );
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        assert!(!run_file(&home, &sup).exists(), "the run file is removed");
    }
}

/// L10.9 (live: super's run waited 10.6 h on its own question): super's question card
/// ends its turn without an answer, a later message runs super while the card is open, and
/// the user's answer arrives as a wake.
#[test]
fn super_s_question_card_never_blocks_its_run() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().canonicalize().unwrap();
    let (home, root, scripts) = (base.join("home"), base.join("projects"), base.join("scripts"));
    for d in [&home, &root, &scripts] {
        std::fs::create_dir_all(d).unwrap();
    }
    let settings = json!({"memory": {"enabled": false}, "notifications": {"enabled": false}});
    std::fs::write(home.join("settings.json"), settings.to_string()).unwrap();
    let question = json!({"kind": "question", "gate": "unclear", "sections": [{"task": "General", "question": {
        "requested": "A name.", "verified_result": "None chosen.",
        "blockers": [{"issue": "Not applicable.", "evidence": "None.", "impact": "None."}],
        "input_needed": "Your taste.", "recommendation": "Atlas.", "risk": "None.",
        "unchanged": "Everything.", "decision": "Which name?"
    }}], "choices": [{"label": "Atlas", "action": "answer"}, {"label": "Orion", "action": "answer"}]});
    let lines: Vec<Value> = [tool("say", question), json!({})]
        .into_iter()
        .chain(tell("still here"))
        .chain(tell("got the name"))
        .collect();
    script(&scripts, "super", &lines);
    let sup = start_super(&home, &root, &scripts);
    let _cleanup = Cleanup(home.clone(), sup.clone());
    let run: Value = serde_json::from_str(&std::fs::read_to_string(run_file(&home, &sup)).unwrap()).unwrap();
    let sock = UnixStream::connect(run["socket"].as_str().unwrap()).unwrap();
    sock.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
    let mut conn = BufReader::new(sock);
    let send = |conn: &mut BufReader<UnixStream>, v: Value| {
        conn.get_mut().write_all(format!("{v}\n").as_bytes()).unwrap();
    };
    send(&mut conn, json!({"id": "p1", "type": "prompt", "text": "name it"}));
    let (mut ends, mut card) = (0, None::<Value>);
    while ends < 3 {
        let mut l = String::new();
        conn.read_line(&mut l)
            .unwrap_or_else(|e| panic!("{e} after {ends} turns; card open: {}", card.is_some()));
        assert!(!l.is_empty(), "super closed the connection");
        let m: Value = serde_json::from_str(&l).unwrap();
        match m["type"].as_str() {
            Some("ask") => card = Some(m),
            Some("event") if m["event"]["type"] == "agent_end" => {
                ends += 1;
                match ends {
                    // The question's turn ended with the card still open: another message runs super.
                    1 => {
                        assert!(card.is_some(), "no card");
                        send(
                            &mut conn,
                            json!({"id": "p2", "type": "prompt", "text": "anything new?"}),
                        );
                    }
                    // Now the user answers.
                    2 => {
                        let m = card.take().unwrap();
                        let q = &m["questions"][0];
                        send(
                            &mut conn,
                            json!({"type": "answer", "ask": m["id"], "replies": [{"question": q["question"], "chosen": ["Orion"]}]}),
                        );
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
    let woken = session_text(&home, &sup);
    assert!(
        woken.contains("The user answered General: Q: Which name? A: Orion"),
        "the answer's wake: {woken}"
    );
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
        .chain(tell("asked to merge"))
        // Wake: the user answered the merge card; the merge started.
        .chain(tell("merging"))
        // Wake: T1 merged; code closes it (L10.9).
        .chain(tell("merged"))
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
            // Code's close summary question, before it closes the merged task (L10.9).
            say("## Implemented\nnote.txt with a greeting.\n## Release/deploy impact\nnone"),
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
    // approval (merge: its card returns at once), the wake on the merge card's answer,
    // the wake on the merge (code closes the task).
    let mut ends = 0;
    let mut relayed: Option<Value> = None;
    let mut merge_card: Option<Value> = None;
    let mut cards = Vec::new();
    let mut said = String::new();
    let deadline = Instant::now() + Duration::from_secs(120);
    while ends < 5 {
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
                    // Answered once super's merge turn ended without it (L10.9).
                    Some(h) if h == format!("{label}: merge") => merge_card = Some(m.clone()),
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
                    3 => {
                        let m = merge_card.take().expect("the merge card is open after super's turn");
                        answer(&mut conn, &m, "Merge");
                    }
                    4 => std::fs::write(&merge_gate, "").unwrap(),
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
    // The merge card's answer and its outcome woke super.
    let woken = session_text(&home, &sup);
    assert!(
        woken.contains(&format!("The user answered {label}: Q: ")) && woken.contains(&format!("{label} is merging")),
        "{woken}"
    );
    assert!(!cards.iter().any(|c| c.contains(": close")), "no close card: {cards:?}");
    // Super's messages are framed by code: the trigger of each run and the task's status.
    assert!(said.contains("\"trigger\":\"reply to you\""), "{said}");
    assert!(
        said.contains(&format!("\"trigger\":\"after {label} reported done\"")),
        "{said}"
    );
    // Code closes the merged task right after that wake.
    let closed = Instant::now() + Duration::from_secs(30);
    let id = task(&home, label)["id"].as_str().unwrap().to_string();
    while task(&home, label)["state"] != "archived" || app.join(".worktrees").join(&id).exists() {
        assert!(Instant::now() < closed, "not closed: {}", board(&home));
        std::thread::sleep(Duration::from_millis(100));
    }
    let t = task(&home, label);
    // Its close summary was asked before it closed, and saved (L10.9).
    let summary = t["close_summary"]
        .as_str()
        .unwrap_or_else(|| panic!("no close summary: {t}"));
    assert!(
        std::fs::read_to_string(summary)
            .unwrap()
            .contains("## Implemented\nnote.txt with a greeting."),
        "{summary}"
    );
    let rv = task(&home, review);
    assert_eq!(rv["state"], "archived", "{rv}");
    assert_eq!(rv["reviewer_of"], t["id"], "{rv}");
    // Merged into main in the main checkout; the merged worktree is gone.
    assert_eq!(std::fs::read_to_string(app.join("note.txt")).unwrap(), "hello\n");
    assert_eq!(git(&app, "branch --show-current"), "main");
    assert!(
        !app.join(".worktrees").join(t["id"].as_str().unwrap()).exists(),
        "worktree removed"
    );
}
