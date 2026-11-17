//! The hidden `faux/script` model: a separate novacode process
//! replays model replies from a JSON Lines file.

use std::path::Path;
use std::process::{Command, Output};

/// `novacode -p <prompt> -m faux/script` in `cwd`, with a fresh home and `env` added.
fn run(cwd: &Path, home: &Path, env: &[(&str, &Path)]) -> Output {
    let mut c = Command::new(env!("CARGO_BIN_EXE_novacode"));
    c.current_dir(cwd)
        .args(["-p", "go", "-m", "faux/script", "--no-session", "--json"])
        .env("NOVACODE_HOME", home)
        .env_remove("NOVACODE_FAUX_SCRIPT");
    for (k, v) in env {
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
    check_two_step(&run(&cwd, &home, &[("NOVACODE_FAUX_SCRIPT", &script)]));
}

/// `--bare` (L8.17): only the answer text, cut at the stop text; no session saved.
#[test]
fn bare_prints_only_the_answer_cut_at_the_stop() {
    let dir = tempfile::tempdir().unwrap();
    let (home, cwd) = (dir.path().join("home"), dir.path().join("work"));
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&cwd).unwrap();
    let script = dir.path().join("script.jsonl");
    std::fs::write(&script, "{\"text\": \"foo(bar)</completion> trailing\"}\n").unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_novacode"))
        .current_dir(&cwd)
        .args([
            "--bare",
            "-p",
            "go",
            "-m",
            "faux/script",
            "--stop",
            "</completion>",
            "--max-tokens",
            "64",
        ])
        .env("NOVACODE_HOME", &home)
        .env("NOVACODE_FAUX_SCRIPT", &script)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(stdout, "foo(bar)");
    assert!(!home.join("sessions").exists(), "no session file");
}
