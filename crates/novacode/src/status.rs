//! `novacode status` (L8.15): the status record of live sessions, as text,
//! JSON or a `--format` template for tmux `#()` and scripts.

use crate::serve::{RunFile, live_sessions};
use novacode_base::paths::Paths;
use std::process::ExitCode;

/// The session meant when none is named: this tmux pane's, else the newest in
/// this folder, else the newest anywhere.
fn pick(all: Vec<RunFile>, session: Option<&str>) -> Vec<RunFile> {
    if let Some(p) = session {
        return all
            .into_iter()
            .filter(|f| f.state.session.starts_with(p))
            .take(1)
            .collect();
    }
    let pane = std::env::var("TMUX_PANE").ok();
    if let Some(f) = all.iter().find(|f| f.pane.is_some() && f.pane == pane) {
        return vec![f.clone()];
    }
    let cwd = std::env::current_dir()
        .map(|d| d.display().to_string())
        .unwrap_or_default();
    let here = all.iter().find(|f| f.state.cwd == cwd).cloned();
    here.or_else(|| all.into_iter().next()).into_iter().collect()
}

fn pct(w: &Option<novacode_ai::quota::Window>) -> String {
    w.as_ref()
        .map(|w| format!("{:.0}%", w.used_pct))
        .unwrap_or_else(|| "-".into())
}

/// `{name}` placeholders: session state detail model account level ctx ctx_tokens window
/// hit read write miss cost 5h week branch project cwd bg.
pub fn render(fmt: &str, f: &RunFile) -> String {
    let s = &f.state;
    let q = s.quota.as_ref();
    let get = |k: &str| -> Option<String> {
        Some(match k {
            "session" => s.session[..s.session.len().min(13)].to_string(),
            "state" => s.activity.state.as_str().into(),
            "detail" => s.activity.detail.clone(),
            "model" => s.model.clone(),
            "account" => s.account.clone().unwrap_or_default(),
            "level" => s.level.as_str().into(),
            "ctx" => s.context_pct().to_string(),
            "ctx_tokens" => s.context_tokens.to_string(),
            "window" => s.window_tokens.to_string(),
            "hit" => format!("{:.0}", s.cache.hit_pct),
            "read" => s.cache.last_read.to_string(),
            "write" => s.cache.last_write.to_string(),
            "miss" => s.cache.last_miss.clone().unwrap_or_default(),
            "cost" => {
                if s.price.is_some() || s.spent > 0.0 {
                    format!("${:.2}", s.spent)
                } else {
                    String::new()
                }
            }
            "5h" => q.map(|q| pct(&q.five_hour)).unwrap_or_default(),
            "week" => q.map(|q| pct(&q.weekly)).unwrap_or_default(),
            "branch" => s.branch.clone().unwrap_or_default(),
            "project" => s.project.clone().unwrap_or_default(),
            "cwd" => s.cwd.clone(),
            "bg" => {
                if f.bg {
                    "bg".into()
                } else {
                    String::new()
                }
            }
            _ => return None,
        })
    };
    let mut out = String::new();
    let mut rest = fmt;
    while let Some(i) = rest.find('{') {
        out.push_str(&rest[..i]);
        let tail = &rest[i + 1..];
        match tail.find('}').and_then(|j| get(&tail[..j]).map(|v| (j, v))) {
            Some((j, v)) => {
                out.push_str(&v);
                rest = &tail[j + 1..];
            }
            None => {
                out.push('{');
                rest = tail;
            }
        }
    }
    out.push_str(rest);
    out
}

const DEFAULT: &str = "{state} · {model}:{level} · ctx {ctx}% · cache {hit}%";

pub fn run(paths: &Paths, session: Option<&str>, all: bool, json: bool, format: Option<&str>) -> ExitCode {
    let live = live_sessions(paths);
    let files = if all { live } else { pick(live, session) };
    if files.is_empty() {
        if !json && format.is_none() {
            eprintln!(
                "no live session{}",
                session.map(|s| format!(" starts with {s}")).unwrap_or_default()
            );
        }
        return ExitCode::from(1);
    }
    for f in &files {
        if json {
            println!("{}", serde_json::to_string(f).unwrap_or_default());
            continue;
        }
        let mut line = render(format.unwrap_or(DEFAULT), f);
        if format.is_none() {
            if let Some(q) = &f.state.quota {
                line += &format!(" · 5h {} · wk {}", pct(&q.five_hour), pct(&q.weekly));
            }
            if f.state.price.is_some() || f.state.spent > 0.0 {
                line += &format!(" · ${:.2}", f.state.spent);
            }
            if all || session.is_none() {
                line = format!("{}  {line}  {}", render("{session}", f), f.state.cwd);
            }
        }
        println!("{line}");
    }
    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::State;

    #[test]
    fn templates() {
        let f = RunFile {
            pid: 1,
            socket: "/s".into(),
            bg: false,
            pane: None,
            started_ms: 0,
            state: State {
                session: "01abcdef-1234-5678".into(),
                model: "claude/claude-opus-5-5".into(),
                account: Some("mili".into()),
                window_tokens: 200_000,
                context_tokens: 50_000,
                ..Default::default()
            },
        };
        assert_eq!(render("{ctx}% {session} {nope} {", &f), "25% 01abcdef-1234 {nope} {");
        assert_eq!(render("{5h}|{cost}", &f), "|");
        assert_eq!(render("{account}", &f), "mili");
        assert!(serde_json::to_string(&f).unwrap().contains(r#""account":"mili""#));
    }
}
