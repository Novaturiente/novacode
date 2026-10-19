//! `novacode import claude` (L7.13): copy Claude Code's hooks, MCP servers,
//! permission rules and default model into novacode's config, once, after showing
//! the changes. Running it again adds nothing. What has no novacode equivalent is
//! listed, never dropped silently.
//!
//! Hooks are wrapped in `novacode claude-hook <event> <matcher> <command>` ([`hook`]),
//! which applies the matcher and hands the command Claude Code's stdin shape, so a
//! copied hook behaves as it did there.

use novacode_base::paths::Paths;
use serde_json::{Map, Value, json};
use std::io::{IsTerminal, Read, Write};
use std::path::Path;
use std::process::ExitCode;

/// Claude Code tool names and novacode's.
pub const TOOLS: &[(&str, &str)] = &[
    ("Read", "read"),
    ("Write", "write"),
    ("Edit", "edit"),
    ("MultiEdit", "edit"),
    ("Bash", "bash"),
    ("Grep", "grep"),
    ("Glob", "find"),
    ("LS", "ls"),
    ("WebFetch", "web_fetch"),
    ("WebSearch", "web_search"),
    ("TodoWrite", "todo"),
    ("Task", "agent"),
];

/// A Claude Code tool name as novacode's (unknown names, e.g. `mcp__…`, unchanged).
pub fn nova_tool(t: &str) -> String {
    TOOLS.iter().find(|(c, _)| *c == t).map_or(t, |(_, n)| n).to_string()
}

fn claude_tool(t: &str) -> String {
    TOOLS.iter().find(|(_, n)| *n == t).map_or(t, |(c, _)| c).to_string()
}

/// One change to one file: set `key` to `value`; `added` is what's new, for the diff.
pub struct Change {
    pub file: &'static str,
    pub key: String,
    pub value: Value,
    pub added: Vec<String>,
}

#[derive(Default)]
pub struct Plan {
    pub changes: Vec<Change>,
    pub skipped: Vec<String>,
}

fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

fn get<'a>(v: &'a Value, dotted: &str) -> &'a Value {
    dotted.split('.').fold(v, |v, k| &v[k])
}

/// Strings at `key` (one string or a list), plus `new` ones not there yet.
fn merge(plan: &mut Plan, file: &'static str, current: &Value, key: &str, new: Vec<String>) {
    let mut list: Vec<String> = match get(current, key) {
        Value::String(s) => vec![s.clone()],
        Value::Array(a) => a.iter().filter_map(|v| v.as_str().map(String::from)).collect(),
        _ => Vec::new(),
    };
    let mut added = Vec::new();
    for n in new {
        if !list.contains(&n) {
            list.push(n.clone());
            added.push(n);
        }
    }
    if !added.is_empty() {
        plan.changes.push(Change {
            file,
            key: key.into(),
            value: json!(list),
            added,
        });
    }
}

/// What to change, from Claude Code's `settings.json` and `~/.claude.json` and
/// novacode's current `settings.json` and `mcp.json`. `alias` turns a Claude model
/// name (`sonnet`) into a novacode route, when one matches.
pub fn plan(
    claude: &Value,
    claude_json: &Value,
    nova: &Value,
    nova_mcp: &Value,
    alias: impl Fn(&str) -> Option<String>,
) -> Plan {
    let mut p = Plan::default();
    // Hooks.
    let mut by_key: Vec<(&str, Vec<String>)> = Vec::new();
    for (event, entries) in claude["hooks"].as_object().into_iter().flatten() {
        let key = match event.as_str() {
            "PreToolUse" => "hooks.beforeTool",
            "PostToolUse" => "hooks.afterTool",
            "SessionStart" => "hooks.events.session_start",
            "SessionEnd" => "hooks.events.session_end",
            "Stop" => "hooks.events.agent_end",
            _ => {
                p.skipped.push(format!(
                    "hook {event}: novacode has no command hook for it (a script extension may cover it)"
                ));
                continue;
            }
        };
        if event == "Stop" {
            p.skipped
                .push("hook Stop: copied as an observer; in novacode it can't keep the agent going".into());
        }
        if event == "SessionStart" {
            p.skipped
                .push("hook SessionStart: copied; its output isn't added to the model's context in novacode".into());
        }
        for entry in entries.as_array().into_iter().flatten() {
            let matcher = entry["matcher"].as_str().unwrap_or_default();
            for h in entry["hooks"].as_array().into_iter().flatten() {
                let Some(cmd) = h["command"].as_str().filter(|_| h["type"] == "command") else {
                    p.skipped.push(format!(
                        "hook {event}: a `{}` hook (only commands are copied)",
                        h["type"]
                    ));
                    continue;
                };
                let line = format!("novacode claude-hook {event} {} {}", sh_quote(matcher), sh_quote(cmd));
                match by_key.iter_mut().find(|(k, _)| *k == key) {
                    Some((_, v)) => v.push(line),
                    None => by_key.push((key, vec![line])),
                }
            }
        }
    }
    for (key, new) in by_key {
        merge(&mut p, "settings.json", nova, key, new);
    }
    // Permission rules: the same `Tool(pattern)` form, with novacode's tool names.
    for kind in ["allow", "ask", "deny"] {
        let mut new = Vec::new();
        for r in claude["permissions"][kind].as_array().into_iter().flatten() {
            let Some(r) = r.as_str() else { continue };
            let (tool, rest) = r.split_once('(').map_or((r, None), |(t, rest)| (t, Some(rest)));
            if tool == "WebFetch" && rest.is_some() {
                p.skipped.push(format!(
                    "permission `{r}`: novacode rules don't match web_fetch by domain"
                ));
                continue;
            }
            new.push(match rest {
                Some(rest) => format!("{}({rest}", nova_tool(tool)),
                None => nova_tool(tool),
            });
        }
        merge(&mut p, "settings.json", nova, &format!("permissions.{kind}"), new);
    }
    if let Some(m) = claude["permissions"]["defaultMode"].as_str() {
        p.skipped.push(format!(
            "permissions.defaultMode `{m}`: novacode keeps its own `permissions.mode`"
        ));
    }
    // MCP servers (the user's own; project ones come from `.mcp.json` as they are).
    for (name, server) in claude_json["mcpServers"].as_object().into_iter().flatten() {
        match nova_mcp["mcpServers"].get(name) {
            Some(mine) if mine == server => {}
            Some(_) => p
                .skipped
                .push(format!("mcp `{name}`: you have one by that name; kept yours")),
            None => p.changes.push(Change {
                file: "mcp.json",
                key: format!("mcpServers.{name}"),
                value: server.clone(),
                added: vec![server.to_string()],
            }),
        }
    }
    // Default model, only when novacode has none.
    if let Some(m) = claude["model"].as_str() {
        if nova.get("model").is_some() {
            p.skipped.push(format!("model `{m}`: you already have a default model"));
        } else if let Some(route) = alias(m) {
            p.changes.push(Change {
                file: "settings.json",
                key: "model".into(),
                value: json!(route),
                added: vec![route],
            });
        } else {
            p.skipped.push(format!("model `{m}`: no matching novacode model"));
        }
    }
    p
}

fn read_json(path: &Path) -> Value {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|t| novacode_base::settings::parse_jsonc(&t))
        .unwrap_or(Value::Object(Map::new()))
}

/// `novacode import claude [--yes]`.
pub fn run(paths: &Paths, providers: &novacode_ai::connect::Providers, yes: bool) -> anyhow::Result<ExitCode> {
    let home = std::path::PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
    let settings = paths.settings();
    let mcp = paths.home().join("mcp.json");
    let plan = plan(
        &read_json(&home.join(".claude/settings.json")),
        &read_json(&home.join(".claude.json")),
        &read_json(&settings),
        &read_json(&mcp),
        |m| {
            // Claude Code's `sonnet`/`opus`/`haiku` are the `claude-*` families.
            let p = providers.profile("claude").ok()?;
            let family = format!("claude-{}", m.split('[').next().unwrap_or(m));
            providers
                .catalog
                .resolve_alias(p.catalog_id(), &family, None)
                .map(|_| format!("claude/{family}"))
        },
    );
    if plan.changes.is_empty() {
        println!("nothing to import: novacode already has everything that maps");
    }
    for c in &plan.changes {
        println!("{} {}", c.file, c.key);
        for a in &c.added {
            println!("  + {a}");
        }
    }
    if !plan.skipped.is_empty() {
        println!("not copied:");
        for s in &plan.skipped {
            println!("  · {s}");
        }
    }
    if plan.changes.is_empty() {
        return Ok(ExitCode::SUCCESS);
    }
    if !yes {
        if !std::io::stdin().is_terminal() {
            println!("run with --yes to apply");
            return Ok(ExitCode::SUCCESS);
        }
        print!("apply these changes? [y/N] ");
        std::io::stdout().flush()?;
        let mut a = String::new();
        std::io::stdin().read_line(&mut a)?;
        if !a.trim().eq_ignore_ascii_case("y") {
            println!("nothing changed");
            return Ok(ExitCode::SUCCESS);
        }
    }
    for f in [&settings, &mcp] {
        if plan.changes.iter().any(|c| f.ends_with(c.file)) && f.exists() {
            std::fs::copy(f, f.with_extension("json.bak"))?;
        }
    }
    for c in &plan.changes {
        let f = if c.file == "mcp.json" { &mcp } else { &settings };
        novacode_base::settings::set(f, &c.key, &c.value)?;
    }
    println!("done; new sessions use it (backups: *.json.bak)");
    Ok(ExitCode::SUCCESS)
}

/// `novacode claude-hook <event> <matcher> <command>`: run a Claude Code hook as a
/// novacode command hook. novacode's stdin JSON (`{tool, args, session, cwd, …}`)
/// becomes Claude Code's (`{tool_name, tool_input, session_id, cwd, hook_event_name}`),
/// the matcher (a regex on the tool name) is applied, and a block (exit 2,
/// `decision: block`, `permissionDecision: deny`) is turned into novacode's.
pub fn hook(event: &str, matcher: &str, command: &str) -> ExitCode {
    let mut stdin = String::new();
    let _ = std::io::stdin().read_to_string(&mut stdin);
    let v: Value = serde_json::from_str(&stdin).unwrap_or_default();
    let tool = v["tool"].as_str().map(claude_tool);
    if let Some(t) = &tool
        && !matcher.is_empty()
        && matcher != "*"
        && !regex::Regex::new(&format!("^(?:{matcher})$")).is_ok_and(|re| re.is_match(t))
    {
        return ExitCode::SUCCESS;
    }
    let mut input = v["args"].clone();
    if let Some(o) = input.as_object_mut()
        && let Some(p) = o.get("path").cloned()
    {
        o.entry("file_path").or_insert(p);
    }
    let mut payload = json!({
        "session_id": v["session"],
        "cwd": v["cwd"],
        "hook_event_name": event,
    });
    if let Some(t) = tool {
        payload["tool_name"] = json!(t);
        payload["tool_input"] = input;
    }
    if event == "PostToolUse" {
        payload["tool_response"] = json!({"output": v["output"], "is_error": v["is_error"]});
    }
    let child = std::process::Command::new("sh")
        .arg("-c")
        .arg(command)
        .env("CLAUDE_PROJECT_DIR", v["cwd"].as_str().unwrap_or("."))
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn();
    let Ok(mut child) = child else {
        return ExitCode::SUCCESS; // fail open, like every novacode hook
    };
    if let Some(mut i) = child.stdin.take() {
        let _ = i.write_all(payload.to_string().as_bytes());
    }
    let Ok(out) = child.wait_with_output() else {
        return ExitCode::SUCCESS;
    };
    let stdout: Value = serde_json::from_slice(&out.stdout).unwrap_or_default();
    let blocked = if out.status.code() == Some(2) {
        Some(String::from_utf8_lossy(&out.stderr).trim().to_string())
    } else if stdout["decision"] == "block" {
        Some(stdout["reason"].as_str().unwrap_or("blocked").to_string())
    } else if stdout["hookSpecificOutput"]["permissionDecision"] == "deny" {
        Some(
            stdout["hookSpecificOutput"]["permissionDecisionReason"]
                .as_str()
                .unwrap_or("denied")
                .to_string(),
        )
    } else {
        None
    };
    match (event, blocked) {
        ("PreToolUse", Some(why)) => {
            eprintln!("{why}");
            ExitCode::from(2)
        }
        // novacode shows an after-tool hook's stdout to the model, as Claude Code does with a block.
        ("PostToolUse", Some(why)) => {
            println!("{why}");
            ExitCode::SUCCESS
        }
        _ => ExitCode::SUCCESS,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plans_once_and_lists_the_rest() {
        let claude = json!({
            "model": "sonnet",
            "hooks": {
                "PreToolUse": [{"matcher": "Bash|Edit", "hooks": [{"type": "command", "command": "check.sh 'x'"}]}],
                "SessionEnd": [{"hooks": [{"type": "command", "command": "bye.sh"}]}],
                "UserPromptSubmit": [{"hooks": [{"type": "command", "command": "ctx.sh"}]}],
            },
            "permissions": {"allow": ["Bash(git add:*)", "Read(src/**)", "Glob", "WebFetch(domain:x.com)"], "defaultMode": "auto"},
        });
        let claude_json = json!({"mcpServers": {"ctx7": {"command": "npx", "args": ["c7"]}, "mine": {"command": "a"}}});
        let nova = json!({"permissions": {"allow": ["bash(git add:*)"]}});
        let nova_mcp = json!({"mcpServers": {"mine": {"command": "b"}}});
        let p = plan(&claude, &claude_json, &nova, &nova_mcp, |m| {
            (m == "sonnet").then(|| "claude/claude-sonnet".into())
        });
        let keys: Vec<&str> = p.changes.iter().map(|c| c.key.as_str()).collect();
        assert_eq!(
            keys,
            [
                "hooks.beforeTool",
                "hooks.events.session_end",
                "permissions.allow",
                "mcpServers.ctx7",
                "model"
            ]
        );
        assert_eq!(
            p.changes[0].added,
            [r"novacode claude-hook PreToolUse 'Bash|Edit' 'check.sh '\''x'\'''"]
        );
        assert_eq!(
            p.changes[2].value,
            json!(["bash(git add:*)", "read(src/**)", "find"]),
            "existing rule kept once"
        );
        let skipped = p.skipped.join("\n");
        for s in ["UserPromptSubmit", "domain:x.com", "defaultMode", "mcp `mine`"] {
            assert!(skipped.contains(s), "{s} in {skipped}");
        }

        // Applied, then planned again: nothing left to do.
        let mut nova = nova.clone();
        let mut mcp = nova_mcp.clone();
        for c in &p.changes {
            let target = if c.file == "mcp.json" { &mut mcp } else { &mut nova };
            let mut cur = target;
            let parts: Vec<&str> = c.key.split('.').collect();
            for k in &parts[..parts.len() - 1] {
                cur = cur.as_object_mut().unwrap().entry(*k).or_insert(json!({}));
            }
            cur[parts[parts.len() - 1]] = c.value.clone();
        }
        let again = plan(&claude, &claude_json, &nova, &mcp, |_| Some("x".into()));
        assert!(
            again.changes.is_empty(),
            "{:?}",
            again.changes.iter().map(|c| &c.key).collect::<Vec<_>>()
        );
    }
}
