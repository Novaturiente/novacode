//! The hidden `faux/script` model (L10.9 build step 1): a separate novacode process
//! replays model replies from a JSON Lines file.

use std::path::Path;
use std::process::{Command, Output};

/// `novacode -p <prompt> -m faux/script` in `cwd`, with a fresh home and `env` added.
fn run(cwd: &Path, home: &Path, env: &[(&str, &Path)], extra: &[(&str, &str)]) -> Output {
    let mut c = Command::new(env!("CARGO_BIN_EXE_novacode"));
    c.current_dir(cwd)
        .args(["-p", "go", "-m", "faux/script", "--no-session", "--json"])
        .env("NOVACODE_HOME", home)
        .env_remove("NOVACODE_FAUX_SCRIPT")
        .env_remove("NOVACODE_FAUX_SCRIPT_DIR")
        .env_remove("NOVACODE_TASK");
    for (k, v) in env {
        c.env(k, v);
    }
    for (k, v) in extra {
        c.env(k, v);
    }
    c.output().unwrap()
}

/// A reply that reads `note.txt`, then a text reply.
const TWO_STEP: &str = concat!(
    r#"{"text": "reading", "tools": [{"name": "read", "args": {"path": "note.txt"}}]}"#,
    "\n",
    r#"{"text": "the note says hello"}"#,
    "\n"
);

fn check_two_step(out: &Output) {
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "stdout: {stdout}\nstderr: {stderr}");
    assert!(stdout.contains("secret-note-body"), "the read tool ran: {stdout}");
    assert!(stdout.contains("the note says hello"), "second reply: {stdout}");
}

#[test]
fn print_mode_replays_a_tool_call_then_text() {
    let dir = tempfile::tempdir().unwrap();
    let (home, cwd) = (dir.path().join("home"), dir.path().join("work"));
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&cwd).unwrap();
    std::fs::write(cwd.join("note.txt"), "secret-note-body").unwrap();
    let script = dir.path().join("script.jsonl");
    std::fs::write(&script, TWO_STEP).unwrap();
    check_two_step(&run(&cwd, &home, &[("NOVACODE_FAUX_SCRIPT", &script)], &[]));
}

#[test]
fn script_dir_picks_the_file_named_after_the_task() {
    let dir = tempfile::tempdir().unwrap();
    let (home, cwd, scripts) = (
        dir.path().join("home"),
        dir.path().join("work"),
        dir.path().join("scripts"),
    );
    for d in [&home, &cwd, &scripts] {
        std::fs::create_dir_all(d).unwrap();
    }
    std::fs::write(cwd.join("note.txt"), "secret-note-body").unwrap();
    std::fs::write(scripts.join("T1-x.jsonl"), TWO_STEP).unwrap();
    std::fs::write(scripts.join("super.jsonl"), "{\"text\": \"i am super\"}\n").unwrap();
    let env = [("NOVACODE_FAUX_SCRIPT_DIR", scripts.as_path())];
    check_two_step(&run(&cwd, &home, &env, &[("NOVACODE_TASK", "T1-x")]));
    let out = run(&cwd, &home, &env, &[]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("i am super"),
        "no task label reads super.jsonl: {stdout}"
    );
}
