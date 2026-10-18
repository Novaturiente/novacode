//! `novacode stats` (L11.11): tables over the dev metrics files.

use novacode_core::metrics::{Record, hit_pct, load};
use novacode_types::Usage;
use std::collections::BTreeMap;
use std::fmt::Write;
use std::path::{Path, PathBuf};

fn secs(ms: u64) -> String {
    match ms {
        ms if ms < 1000 => format!("{ms}ms"),
        ms if ms < 60_000 => format!("{:.1}s", ms as f64 / 1000.0),
        ms => format!("{}m{:02}s", ms / 60_000, ms % 60_000 / 1000),
    }
}

fn add(a: &mut Usage, b: &Usage) {
    a.input += b.input;
    a.output += b.output;
    a.cache_read += b.cache_read;
    a.cache_write += b.cache_write;
}

#[derive(Default)]
struct ToolAgg {
    calls: u32,
    errors: u32,
    total: u64,
    max: u64,
    out_tokens: u64,
}

fn tools_table(recs: &[&Record], out: &mut String) {
    let mut by: BTreeMap<&str, ToolAgg> = BTreeMap::new();
    for r in recs {
        if let Record::Tool {
            name,
            duration_ms,
            out_tokens,
            is_error,
            ..
        } = r
        {
            let a = by.entry(name).or_default();
            a.calls += 1;
            a.errors += *is_error as u32;
            a.total += duration_ms;
            a.max = a.max.max(*duration_ms);
            a.out_tokens += out_tokens;
        }
    }
    if by.is_empty() {
        return;
    }
    let mut rows: Vec<_> = by.into_iter().collect();
    rows.sort_by_key(|(_, a)| std::cmp::Reverse(a.total));
    let _ = writeln!(out, "\nTools (by total time)");
    let _ = writeln!(
        out,
        "  {:<16} {:>6} {:>6} {:>9} {:>9} {:>9} {:>10}",
        "tool", "calls", "errors", "total", "avg", "max", "out tokens"
    );
    for (name, a) in rows {
        let _ = writeln!(
            out,
            "  {:<16} {:>6} {:>6} {:>9} {:>9} {:>9} {:>10}",
            name,
            a.calls,
            a.errors,
            secs(a.total),
            secs(a.total / a.calls.max(1) as u64),
            secs(a.max),
            a.out_tokens
        );
    }
}

fn turns_table(turns: &[&Record], out: &mut String) {
    let _ = writeln!(
        out,
        "  {:>7} {:>8} {:>8} {:>7} {:>8} {:>7} {:>8} {:>6} {:>8} {:>8} {:>5} {:>6} {:>8}",
        "task.tn",
        "wall",
        "model",
        "ttft",
        "thinking",
        "text",
        "tool-arg",
        "wait",
        "tools",
        "overhead",
        "calls",
        "hit%",
        "ctx"
    );
    for r in turns {
        if let Record::Turn {
            task,
            turn,
            wall_ms,
            model_ms,
            ttft_ms,
            thinking_ms,
            text_ms,
            tool_args_ms,
            preflight_ms,
            tools_ms,
            overhead_ms,
            tool_calls,
            hit_pct,
            ctx_tokens,
            ..
        } = r
        {
            let _ = writeln!(
                out,
                "  {:>7} {:>8} {:>8} {:>7} {:>8} {:>7} {:>8} {:>6} {:>8} {:>8} {tool_calls:>5} {hit_pct:>6.1} {ctx_tokens:>8}",
                format!("{task}.{turn}"),
                secs(*wall_ms),
                secs(*model_ms),
                ttft_ms.map(secs).unwrap_or("-".into()),
                secs(*thinking_ms),
                secs(*text_ms),
                secs(*tool_args_ms),
                secs(*preflight_ms),
                secs(*tools_ms),
                secs(*overhead_ms),
            );
        }
    }
}

/// Report of one session's records; `all_turns` lists every turn instead of the slowest.
pub fn session_report(recs: &[Record], all_turns: bool) -> String {
    let mut out = String::new();
    let all: Vec<&Record> = recs.iter().collect();
    let mut usage = Usage::default();
    let (mut wall, mut model, mut tools, mut pre) = (0, 0, 0, 0);
    let tasks: Vec<&Record> = all
        .iter()
        .copied()
        .filter(|r| matches!(r, Record::Task { .. }))
        .collect();
    for t in &tasks {
        if let Record::Task {
            usage: u,
            wall_ms,
            model_ms,
            tools_ms,
            preflight_ms,
            ..
        } = t
        {
            add(&mut usage, u);
            wall += wall_ms;
            model += model_ms;
            tools += tools_ms;
            pre += preflight_ms;
        }
    }
    if let Some(Record::Session {
        session, cwd, window, ..
    }) = all.first()
    {
        let _ = writeln!(out, "session {session}  {cwd}  window {}k", window / 1000);
    }
    let other = wall.saturating_sub(model + tools + pre);
    let idle: u64 = tasks
        .iter()
        .filter_map(|t| match t {
            Record::Task { idle_ms, .. } => *idle_ms,
            _ => None,
        })
        .sum();
    let _ = writeln!(
        out,
        "{} tasks · {} busy (model {}, tools {}, approvals/checks {}, other {}) · {} idle between tasks · prompt {} tokens, {:.1}% from cache · output {}",
        tasks.len(),
        secs(wall),
        secs(model),
        secs(tools),
        secs(pre),
        secs(other),
        secs(idle),
        usage.prompt_total(),
        hit_pct(&usage),
        usage.output
    );
    if !tasks.is_empty() {
        let _ = writeln!(out, "\nTasks");
        let _ = writeln!(
            out,
            "  {:>3} {:>8} {:>8} {:>8} {:>8} {:>6} {:>5} {:>5} {:>4} {:>5} {:>6} {:>8}  prompt",
            "#", "idle", "wall", "model", "tools", "wait", "turns", "calls", "err", "retry", "hit%", "ctx"
        );
        for t in &tasks {
            if let Record::Task {
                task,
                idle_ms,
                prompt_preview,
                wall_ms,
                model_ms,
                tools_ms,
                preflight_ms,
                turns,
                tool_calls,
                tool_errors,
                retries,
                hit_pct,
                ctx_tokens,
                stop,
                ..
            } = t
            {
                let p: String = prompt_preview.chars().take(50).collect();
                let stop = if stop == "stop" {
                    String::new()
                } else {
                    format!(" [{stop}]")
                };
                let _ = writeln!(
                    out,
                    "  {task:>3} {:>8} {:>8} {:>8} {:>8} {:>6} {turns:>5} {tool_calls:>5} {tool_errors:>4} {retries:>5} {hit_pct:>6.1} {ctx_tokens:>8}  {p}{stop}",
                    idle_ms.map(secs).unwrap_or("-".into()),
                    secs(*wall_ms),
                    secs(*model_ms),
                    secs(*tools_ms),
                    secs(*preflight_ms),
                );
            }
        }
    }
    let mut turns: Vec<&Record> = all
        .iter()
        .copied()
        .filter(|r| matches!(r, Record::Turn { .. }))
        .collect();
    if !turns.is_empty() {
        let mut split = [0u64; 7];
        for r in &turns {
            if let Record::Turn {
                thinking_ms,
                text_ms,
                tool_args_ms,
                model_ms,
                preflight_ms,
                tools_ms,
                overhead_ms,
                ..
            } = r
            {
                let wait = model_ms.saturating_sub(thinking_ms + text_ms + tool_args_ms);
                for (i, v) in [
                    *thinking_ms,
                    *text_ms,
                    *tool_args_ms,
                    wait,
                    *preflight_ms,
                    *tools_ms,
                    *overhead_ms,
                ]
                .into_iter()
                .enumerate()
                {
                    split[i] += v;
                }
            }
        }
        let sum: u64 = split.iter().sum::<u64>().max(1);
        let names = [
            "thinking",
            "text",
            "tool args",
            "model wait",
            "approvals/checks",
            "tools",
            "overhead",
        ];
        let parts: Vec<String> = names
            .iter()
            .zip(split)
            .map(|(n, v)| format!("{n} {} ({:.0}%)", secs(v), v as f64 * 100.0 / sum as f64))
            .collect();
        let _ = writeln!(
            out,
            "\nWhere turn time went ({} turns)\n  {}",
            turns.len(),
            parts.join(" · ")
        );
        if all_turns {
            let _ = writeln!(out, "\nTurns");
        } else {
            turns.sort_by_key(|r| match r {
                Record::Turn { wall_ms, .. } => std::cmp::Reverse(*wall_ms),
                _ => std::cmp::Reverse(0),
            });
            turns.truncate(10);
            let _ = writeln!(out, "\nSlowest turns (--turns for all)");
        }
        turns_table(&turns, &mut out);
    }
    tools_table(&all, &mut out);
    let mut calls: Vec<&Record> = all
        .iter()
        .copied()
        .filter(|r| matches!(r, Record::Model { .. }))
        .collect();
    calls.sort_by_key(|r| match r {
        Record::Model { duration_ms, .. } => std::cmp::Reverse(*duration_ms),
        _ => std::cmp::Reverse(0),
    });
    if !calls.is_empty() {
        let _ = writeln!(out, "\nSlowest model calls");
        let _ = writeln!(
            out,
            "  {:>7} {:<32} {:>8} {:>8} {:>6} {:>8} {:>7} {:>5}",
            "task.tn", "model", "ttft", "total", "hit%", "prompt", "output", "retry"
        );
        for r in calls.iter().take(8) {
            if let Record::Model {
                task,
                turn,
                provider,
                model,
                ttft_ms,
                duration_ms,
                usage,
                hit_pct,
                retries,
                ..
            } = r
            {
                let _ = writeln!(
                    out,
                    "  {:>7} {:<32} {:>8} {:>8} {hit_pct:>6.1} {:>8} {:>7} {retries:>5}",
                    format!("{task}.{turn}"),
                    format!("{provider}/{model}"), // provider = the reply's API family, not the login
                    ttft_ms.map(secs).unwrap_or("-".into()),
                    secs(*duration_ms),
                    usage.prompt_total(),
                    usage.output,
                );
            }
        }
    }
    let notes: Vec<String> = all
        .iter()
        .filter_map(|r| match r {
            Record::Notice { task, text, .. } => Some(format!("  task {task}: {text}")),
            Record::Retry {
                task,
                turn,
                error,
                message,
                ..
            } => Some(format!("  task {task}.{turn}: retry after {error}: {message}")),
            _ => None,
        })
        .collect();
    if !notes.is_empty() {
        let _ = writeln!(out, "\nNotices and retries\n{}", notes.join("\n"));
    }
    out
}

/// One row per session, then tools over all sessions.
pub fn all_report(files: &[(PathBuf, Vec<Record>)]) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "  {:<10} {:>5} {:>8} {:>8} {:>8} {:>6} {:>10} {:>6}  cwd",
        "session", "tasks", "busy", "model", "tools", "calls", "prompt", "hit%"
    );
    let mut every: Vec<&Record> = Vec::new();
    for (_, recs) in files {
        let mut u = Usage::default();
        let (mut n, mut wall, mut model, mut tools, mut calls) = (0, 0, 0, 0, 0);
        for r in recs {
            if let Record::Task {
                usage,
                wall_ms,
                model_ms,
                tools_ms,
                tool_calls,
                ..
            } = r
            {
                n += 1;
                add(&mut u, usage);
                wall += wall_ms;
                model += model_ms;
                tools += tools_ms;
                calls += tool_calls;
            }
        }
        let (id, cwd) = match recs.first() {
            Some(Record::Session { session, cwd, .. }) => (session.chars().take(10).collect(), cwd.clone()),
            _ => (String::from("?"), String::new()),
        };
        let _ = writeln!(
            out,
            "  {id:<10} {n:>5} {:>8} {:>8} {:>8} {calls:>6} {:>10} {:>6.1}  {cwd}",
            secs(wall),
            secs(model),
            secs(tools),
            u.prompt_total(),
            hit_pct(&u)
        );
        every.extend(recs);
    }
    tools_table(&every, &mut out);
    out
}

/// Metrics files, newest first.
pub fn files(home: &Path) -> Vec<PathBuf> {
    let mut v: Vec<(std::time::SystemTime, PathBuf)> = std::fs::read_dir(home.join("metrics"))
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "jsonl"))
        .map(|e| {
            (
                e.metadata().and_then(|m| m.modified()).unwrap_or(std::time::UNIX_EPOCH),
                e.path(),
            )
        })
        .collect();
    v.sort_by_key(|(t, _)| std::cmp::Reverse(*t));
    v.into_iter().map(|(_, p)| p).collect()
}

/// `novacode stats [session] [--all]`; exit code 1 when nothing matches.
pub fn run(home: &Path, session: Option<&str>, all: bool, all_turns: bool) -> i32 {
    let files = files(home);
    if files.is_empty() {
        eprintln!("no metrics yet: set \"debug\": {{\"metrics\": true}} in settings.json, then run a session");
        return 1;
    }
    if all {
        let loaded: Vec<_> = files
            .into_iter()
            .map(|p| {
                let r = load(&p);
                (p, r)
            })
            .collect();
        print!("{}", all_report(&loaded));
        return 0;
    }
    let pick = match session {
        None => files.into_iter().next(),
        Some(prefix) => files.into_iter().find(|p| {
            p.file_stem()
                .and_then(|s| s.to_str())
                .is_some_and(|s| s.starts_with(prefix))
        }),
    };
    match pick {
        Some(p) => {
            print!("{}", session_report(&load(&p), all_turns));
            0
        }
        None => {
            eprintln!("no metrics for session `{}`", session.unwrap_or_default());
            1
        }
    }
}
