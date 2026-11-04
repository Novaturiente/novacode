//! `novacode --super` entry (L10.9 "Entry and scope"): one super per root, launchers attach.
//! Without a terminal the launcher prints the super's session id instead of attaching.

use std::path::Path;
use std::process::{Command, Output};

fn launch(cwd: &Path, home: &Path, extra: &[&str]) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_novacode"));
    c.current_dir(cwd)
        .arg("--super")
        .args(extra)
        .env("NOVACODE_HOME", home)
        .env_remove("NOVACODE_BG_CHILD")
        .env_remove("NOVACODE_FAUX_SCRIPT")
        .env_remove("NOVACODE_FAUX_SCRIPT_DIR")
        .env_remove("NOVACODE_TASK")
        .stdin(std::process::Stdio::null());
    c
}

fn text(o: &Output) -> String {
    format!(
        "status {:?}\nstdout: {}\nstderr: {}",
        o.status,
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    )
}

fn stop(home: &Path, id: &str) {
    let _ = Command::new(env!("CARGO_BIN_EXE_novacode"))
        .args(["stop", id])
        .env("NOVACODE_HOME", home)
        .output();
}

#[test]
fn two_concurrent_launches_give_one_super_and_both_attach() {
    let dir = tempfile::tempdir().unwrap();
    let (home, root) = (dir.path().join("home"), dir.path().join("projects"));
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(root.join("app")).unwrap();
    let script = dir.path().join("super.jsonl");
    std::fs::write(&script, "").unwrap();
    let args = ["-m", "faux/script"];
    let spawn = || {
        launch(&root, &home, &args)
            .env("NOVACODE_FAUX_SCRIPT", &script)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap()
    };
    let (a, b) = (spawn(), spawn());
    let (a, b) = (a.wait_with_output().unwrap(), b.wait_with_output().unwrap());
    let ida = String::from_utf8_lossy(&a.stdout).trim().to_string();
    let idb = String::from_utf8_lossy(&b.stdout).trim().to_string();
    let cleanup = |id: &str| stop(&home, id);
    if !a.status.success() || !b.status.success() || ida.is_empty() || ida != idb {
        cleanup(&ida);
        cleanup(&idb);
        panic!("a: {}\nb: {}", text(&a), text(&b));
    }
    // Exactly one live super process for the root.
    let agents = Command::new(env!("CARGO_BIN_EXE_novacode"))
        .arg("agents")
        .env("NOVACODE_HOME", &home)
        .output()
        .unwrap();
    let list = String::from_utf8_lossy(&agents.stdout).to_string();
    let live = list.lines().skip(1).filter(|l| !l.trim().is_empty()).count();
    // A third launch reattaches to the same super; a nested one is refused.
    let c = launch(&root, &home, &[]).output().unwrap();
    let nested = launch(&root.join("app"), &home, &[]).output().unwrap();
    cleanup(&ida);
    assert_eq!(live, 1, "one super runs: {list}");
    assert_eq!(String::from_utf8_lossy(&c.stdout).trim(), ida, "{}", text(&c));
    assert!(!nested.status.success(), "{}", text(&nested));
    assert!(
        String::from_utf8_lossy(&nested.stderr).contains("inside"),
        "{}",
        text(&nested)
    );
}

#[test]
fn refused_flag_combinations_start_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    for bad in [&["--bg"][..], &["-c"], &["--trust"], &["-w", "x"]] {
        let o = launch(dir.path(), &home, bad).output().unwrap();
        assert!(!o.status.success(), "{bad:?}: {}", text(&o));
    }
    assert!(!home.join("super").exists(), "nothing was started");
}
