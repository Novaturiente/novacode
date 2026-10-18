//! Diagnostics (L11.3): crash log, startup profile and `novacode debug bundle`.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

use novacode_base::paths::Paths;
use novacode_base::redact::Redactor;

// ---- crash log ----

/// On a panic, write `<logs>/crash-<unix ms>-<pid>.log`: the message, a backtrace
/// and the tail of the log, secrets masked. The TUI's hook restores the terminal
/// first and then calls this one.
pub fn install_crash_hook(paths: &Paths) {
    let logs = paths.logs();
    let redactor = crate::export_redactor(paths);
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let bt = std::backtrace::Backtrace::force_capture();
        let thread = std::thread::current().name().unwrap_or("?").to_string();
        let text = format!(
            "novacode {} crashed\nargs: {}\nthread: {thread}\n{info}\n\nbacktrace:\n{bt}\n\nlast log lines:\n{}",
            env!("CARGO_PKG_VERSION"),
            args(),
            log_tail(&logs, 40),
        );
        let text = scrub(&redactor, &text);
        let file = logs.join(format!("crash-{}-{}.log", now_ms(), std::process::id()));
        if std::fs::write(&file, text).is_ok() {
            eprintln!("novacode crashed; details in {}", file.display());
        }
        prev(info);
    }));
}

/// The command line, `--api-key` values hidden.
fn args() -> String {
    let mut out = Vec::new();
    let mut hide = false;
    for a in std::env::args() {
        if hide {
            out.push("[redacted]".into());
            hide = false;
            continue;
        }
        hide = a == "--api-key";
        out.push(if a.starts_with("--api-key=") {
            "--api-key=[redacted]".into()
        } else {
            a
        });
    }
    out.join(" ")
}

fn now_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis())
}

/// The newest `novacode.log*` file's last `n` lines.
fn log_tail(logs: &Path, n: usize) -> String {
    let newest = newest(logs, "novacode.log", 1);
    let Some(f) = newest.first() else { return String::new() };
    tail(f, 256 * 1024)
        .lines()
        .rev()
        .take(n)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<Vec<_>>()
        .join("\n")
}

/// Files in `dir` whose name starts with `prefix`, newest first, at most `n`.
fn newest(dir: &Path, prefix: &str, n: usize) -> Vec<PathBuf> {
    let mut v: Vec<(std::time::SystemTime, PathBuf)> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with(prefix))
        .filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path())))
        .collect();
    v.sort_by_key(|x| std::cmp::Reverse(x.0));
    v.into_iter().take(n).map(|x| x.1).collect()
}

/// The last `max` bytes of a file, cut at a line start.
fn tail(f: &Path, max: u64) -> String {
    use std::io::{Read, Seek, SeekFrom};
    let Ok(mut file) = std::fs::File::open(f) else {
        return String::new();
    };
    let len = file.metadata().map_or(0, |m| m.len());
    let _ = file.seek(SeekFrom::Start(len.saturating_sub(max)));
    let mut buf = Vec::new();
    let _ = file.read_to_end(&mut buf);
    let s = String::from_utf8_lossy(&buf).into_owned();
    if len > max {
        s.split_once('\n').map_or(s.clone(), |(_, rest)| rest.to_string())
    } else {
        s
    }
}

/// Known secrets (logins, keys) and anything shaped like a credential.
pub fn scrub(redactor: &Redactor, text: &str) -> String {
    static PATTERNS: OnceLock<Vec<regex::Regex>> = OnceLock::new();
    let pats = PATTERNS.get_or_init(|| {
        [
            r"sk-[A-Za-z0-9_\-]{16,}",
            r"(?i)bearer\s+[A-Za-z0-9._~+/=\-]{16,}",
            r"gh[pousr]_[A-Za-z0-9]{20,}",
            r"github_pat_[A-Za-z0-9_]{20,}",
            r"AIza[0-9A-Za-z_\-]{30,}",
            r"ya29\.[0-9A-Za-z_\-]{20,}",
            r"eyJ[A-Za-z0-9_\-]{10,}\.[A-Za-z0-9_\-]{10,}\.[A-Za-z0-9_\-]{10,}",
            // "apiKey": "...", "token": "...", access_token=... in JSON or query strings
            r#"(?i)("?(?:api_?key|access_?token|refresh_?token|token|secret|password|client_secret|authorization)"?\s*[:=]\s*"?)[^"\s,}&]{8,}"#,
        ]
        .iter()
        .map(|p| regex::Regex::new(p).expect("pattern"))
        .collect()
    });
    let mut s = redactor.apply(text).unwrap_or_else(|| text.to_string());
    for (i, re) in pats.iter().enumerate() {
        let rep = if i == pats.len() - 1 {
            "${1}[redacted]"
        } else {
            "[redacted]"
        };
        s = re.replace_all(&s, rep).into_owned();
    }
    s
}

// ---- startup profile ----

static MARKS: Mutex<Vec<(&'static str, Instant)>> = Mutex::new(Vec::new());
static START: OnceLock<Instant> = OnceLock::new();

/// Note that a startup phase ended (`--profile-startup`).
pub fn mark(name: &'static str) {
    START.get_or_init(Instant::now);
    MARKS.lock().unwrap().push((name, Instant::now()));
}

/// The phases so far, each with its own time and the running total.
pub fn profile() -> String {
    let start = *START.get_or_init(Instant::now);
    let mut prev = start;
    let mut s = String::from("startup phase                 took     total\n");
    for (name, at) in MARKS.lock().unwrap().iter() {
        s.push_str(&format!(
            "  {name:<26} {:>6.1}ms {:>7.1}ms\n",
            at.duration_since(prev).as_secs_f64() * 1e3,
            at.duration_since(start).as_secs_f64() * 1e3
        ));
        prev = *at;
    }
    s
}

// ---- debug bundle ----

/// Gather a redacted bug-report folder for `session` (or none) into `out`.
/// Returns the files written, relative to `out`.
pub fn gather(paths: &Paths, cwd: &Path, session: Option<&Path>, out: &Path) -> anyhow::Result<Vec<String>> {
    let redactor = crate::export_redactor(paths);
    std::fs::create_dir_all(out)?;
    let mut written = Vec::new();
    let mut put = |name: &str, text: &str| -> anyhow::Result<()> {
        let f = out.join(name);
        if let Some(d) = f.parent() {
            std::fs::create_dir_all(d)?;
        }
        std::fs::write(&f, scrub(&redactor, text))?;
        written.push(name.to_string());
        Ok(())
    };
    let home = paths.home();
    put("info.txt", &info(paths, cwd))?;
    for (name, f) in [
        ("settings.json", paths.settings()),
        ("project-settings.json", Paths::project_settings(cwd)),
        ("keybindings.json", home.join("keybindings.json")),
        ("mcp.json", home.join("mcp.json")),
    ] {
        if let Ok(t) = std::fs::read_to_string(&f) {
            put(name, &t)?;
        }
    }
    let logs = paths.logs();
    for (prefix, n, max) in [
        ("novacode.log", 3, 2 << 20),
        ("crash-", 5, 1 << 20),
        ("cache-misses", 1, 256 << 10),
        ("mcp-", 10, 256 << 10),
        ("ext-", 10, 256 << 10),
    ] {
        for f in newest(&logs, prefix, n) {
            let name = format!("logs/{}", f.file_name().unwrap_or_default().to_string_lossy());
            put(&name, &tail(&f, max))?;
        }
    }
    if let Some(s) = session {
        put("session.jsonl", &std::fs::read_to_string(s)?)?;
        let id = s.file_stem().unwrap_or_default().to_string_lossy();
        if let Ok(t) = std::fs::read_to_string(home.join("metrics").join(format!("{id}.jsonl"))) {
            put("metrics.jsonl", &t)?;
        }
    }
    Ok(written)
}

fn info(paths: &Paths, cwd: &Path) -> String {
    let uname = std::process::Command::new("uname")
        .arg("-a")
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();
    let env: Vec<String> = ["TERM", "TERM_PROGRAM", "COLORTERM", "SHELL", "LANG"]
        .iter()
        .map(|k| format!("{k}={}", std::env::var(k).unwrap_or_default()))
        .chain(
            ["TMUX", "SSH_CONNECTION", "WAYLAND_DISPLAY", "HERDR_ENV"]
                .iter()
                .map(|k| format!("{k} {}", if std::env::var_os(k).is_some() { "set" } else { "unset" })),
        )
        .collect();
    let mut logins: Vec<String> = novacode_ai::auth::AuthStore::new(paths.home().join("auth.json"))
        .logins()
        .into_iter()
        .map(|(p, a, _)| format!("{p}:{a}"))
        .collect();
    logins.dedup();
    format!(
        "novacode {}\n{uname}\ncwd {}\nhome {}\nlogins: {}\n{}\n",
        env!("CARGO_PKG_VERSION"),
        cwd.display(),
        paths.home().display(),
        logins.join(", "),
        env.join("\n")
    )
}

/// `novacode debug bundle`: gather, then pack as `.tar.gz` with the system `tar`.
pub fn bundle(paths: &Paths, cwd: &Path, session: Option<&Path>, out: Option<PathBuf>) -> anyhow::Result<PathBuf> {
    let name = format!("novacode-debug-{}", now_ms());
    let tmp = std::env::temp_dir().join(&name);
    let files = gather(paths, cwd, session, &tmp)?;
    let out = out.unwrap_or_else(|| cwd.join(format!("{name}.tar.gz")));
    let st = std::process::Command::new("tar")
        .arg("czf")
        .arg(&out)
        .arg("-C")
        .arg(std::env::temp_dir())
        .arg(&name)
        .status()?;
    let _ = std::fs::remove_dir_all(&tmp);
    anyhow::ensure!(st.success(), "tar failed ({st})");
    eprintln!("{} files, secrets masked:\n  {}", files.len(), files.join("\n  "));
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundle_holds_no_secrets() {
        let home = std::env::temp_dir().join(format!("nc-diag-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(home.join("logs")).unwrap();
        let login = "sk-ant-oat01-SECRETSECRETSECRETSECRET";
        std::fs::write(
            home.join("auth.json"),
            format!(
                r#"{{"version":1,"oauth":{{"claude":{{"default":{{"access":"{login}","refresh":"rfr-0123456789abcdef","expires_ms":0}}}}}},"keys":{{"openrouter":"or-key-literal-0123456789"}}}}"#
            ),
        )
        .unwrap();
        std::fs::write(
            home.join("settings.json"),
            r#"{"providers":{"x":{"apiKey":"plainkey12345678"}}}"#,
        )
        .unwrap();
        std::fs::write(
            home.join("logs/novacode.log.2026-10-18"),
            format!("request with {login} and Bearer abcdefghijklmnopqrstuvwxyz and or-key-literal-0123456789\nGOOGLE AIzaSyA0123456789abcdefghijklmnopqrstu\n"),
        )
        .unwrap();
        let session = home.join("s.jsonl");
        std::fs::write(&session, format!("{{\"text\":\"token={login}\"}}\n")).unwrap();
        let paths = Paths::new(home.clone());
        let out = home.join("out");
        let files = gather(&paths, &home, Some(&session), &out).unwrap();
        assert!(files.contains(&"logs/novacode.log.2026-10-18".to_string()) && files.contains(&"session.jsonl".into()));
        for f in &files {
            let t = std::fs::read_to_string(out.join(f)).unwrap();
            for secret in [
                "SECRETSECRET",
                "rfr-0123456789abcdef",
                "or-key-literal",
                "plainkey12345678",
                "abcdefghijklmnop",
                "AIzaSy",
            ] {
                assert!(!t.contains(secret), "{f} leaks {secret}: {t}");
            }
        }
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn profile_lists_phases_in_order() {
        mark("a");
        mark("b");
        let p = profile();
        assert!(p.find("  a ").unwrap() < p.find("  b ").unwrap(), "{p}");
    }
}
