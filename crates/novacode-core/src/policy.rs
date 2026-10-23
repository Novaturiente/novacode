//! Permissions (L6.1), the risk gate's verdicts (L6.2) and tool profiles
//! (L6.4). Checked in the tool pipeline for every call, so no call path can
//! skip it.
//!
//! Order: profile → deny rules → risk gate → read-only mode → allow rules →
//! hook allow → ask rules → ask mode → allow, then a hook ask turns an allow
//! into an ask. Catastrophic commands and protected commands are refused
//! before any allow rule is looked at.

use crate::risk::{self, Level};
use futures_util::future::BoxFuture;
use novacode_base::settings;
use serde::Deserialize;
use serde_json::Value;
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Mode {
    /// Everything runs unless the risk gate stops it (default).
    #[default]
    Auto,
    /// The user approves edits and non-read-only bash.
    Ask,
    /// No edits; bash only for read-only commands (plan mode).
    ReadOnly,
}

impl Mode {
    /// The `--mode` value.
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Auto => "auto",
            Mode::Ask => "ask",
            Mode::ReadOnly => "read-only",
        }
    }
}

/// `permissions` in global settings (never project settings, L0.6).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Permissions {
    pub mode: Mode,
    pub allow: Vec<String>,
    pub ask: Vec<String>,
    pub deny: Vec<String>,
    pub protected: Protected,
    pub profiles: BTreeMap<String, Profile>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Protected {
    /// Command names that never run, e.g. `sudo`.
    pub commands: Vec<String>,
    /// Paths treated like credentials by the risk gate.
    pub paths: Vec<String>,
}

/// A named tool set (L6.4), e.g. `scout = read, grep, find, ls`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Profile {
    /// Only these tools; `None` = all.
    pub tools: Option<Vec<String>>,
    pub deny: Vec<String>,
}

/// `Tool` or `Tool(pattern)`. Bash patterns match a simple command exactly, or
/// by prefix with a trailing `:*`; other tools glob-match their `path` argument.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rule {
    tool: String,
    pattern: Option<String>,
}

impl Rule {
    pub fn parse(s: &str) -> Option<Self> {
        let s = s.trim();
        let (tool, pattern) = match s.split_once('(') {
            Some((t, rest)) => (t, Some(rest.strip_suffix(')')?.to_string())),
            None => (s, None),
        };
        (!tool.is_empty()).then(|| Self {
            tool: tool.to_lowercase(),
            pattern,
        })
    }

    fn matches_tool(&self, tool: &str) -> bool {
        self.tool == "*" || self.tool == tool.to_lowercase()
    }

    fn matches_command(&self, segment: &str) -> bool {
        match self.pattern.as_deref() {
            None | Some("*") => true,
            Some(p) => match p.strip_suffix(":*") {
                Some(prefix) => segment == prefix || segment.starts_with(&format!("{prefix} ")),
                None => segment == p,
            },
        }
    }

    fn matches_path(&self, raw: Option<&str>, abs: Option<&Path>) -> bool {
        let Some(p) = self.pattern.as_deref() else { return true };
        let Ok(glob) = globset::Glob::new(p) else { return false };
        let m = glob.compile_matcher();
        raw.is_some_and(|r| m.is_match(r)) || abs.is_some_and(|a| m.is_match(a))
    }
}

impl std::fmt::Display for Rule {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut name = self.tool.clone();
        if let Some(c) = name.get_mut(0..1) {
            c.make_ascii_uppercase();
        }
        match &self.pattern {
            Some(p) => write!(f, "{name}({p})"),
            None => f.write_str(&name),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Allow,
    /// Needs the user. `rule` is what "always allow" would save; `risky` = held by the risk gate.
    Ask {
        reason: String,
        rule: String,
        risky: bool,
    },
    /// Refused; the text goes to the model as the tool result.
    Deny(String),
}

/// A `PreToolUse` hook's permission answer (L7.16). Its deny is a hook block, never this.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum HookDecision {
    #[default]
    None,
    /// Skips ask rules and ask mode only.
    Allow,
    /// Needs the user even when the policy allows it; the text is the reason.
    Ask(String),
}

impl HookDecision {
    /// Several hooks answered: ask wins over allow; the first ask's reason is kept.
    pub fn merge(self, other: Self) -> Self {
        match (self, other) {
            (a @ Self::Ask(_), _) | (_, a @ Self::Ask(_)) => a,
            (Self::Allow, _) | (_, Self::Allow) => Self::Allow,
            _ => Self::None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ApprovalRequest {
    pub tool: String,
    pub args: Value,
    pub reason: String,
    /// The rule shown on the card and saved by [`Approval::Always`].
    pub rule: String,
    pub risky: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Approval {
    Once,
    Session,
    /// Save `rule` to global settings. For risk-gated commands this acts like `Session`.
    Always,
    Deny(Option<String>),
}

/// Whoever answers approval cards: the TUI (M4), an RPC client, a parent agent.
pub trait Approver: Send + Sync {
    fn approve(&self, req: ApprovalRequest) -> BoxFuture<'_, Approval>;
}

const MUTATING: &[&str] = &["write", "edit", "apply_patch"];
/// Built-in tools that only read, or change only the session itself; they run in
/// read-only mode. `process` start is checked as its command, `agent` children
/// inherit the mode, and `codemode`/`call` calls are checked one by one.
const READ_TOOLS: &[&str] = &[
    "read",
    "grep",
    "find",
    "ls",
    "todo",
    "tool_search",
    "call",
    "ask_user",
    "web_fetch",
    "web_search",
    "session_search",
    "recall",
    "memory",
    "schedule",
    "goal",
    "plan_complete",
    "enter_plan_mode",
    "agent",
    "codemode",
    "classify",
    "process",
];
const FILE_TOOLS: &[&str] = &["read", "write", "edit", "grep", "find", "ls"];
/// A justification shorter than this is a blind retry, not a reason.
const MIN_JUSTIFICATION: usize = 12;

/// A mode preset's rules: (name, read-only, only these tools).
pub type Overlay = (String, bool, Option<Vec<String>>);

#[derive(Debug)]
pub struct Policy {
    perms: Permissions,
    allow: Vec<Rule>,
    ask: Vec<Rule>,
    deny: Vec<Rule>,
    profile: Profile,
    home: PathBuf,
    /// Global settings file, for "always allow".
    global_settings: Option<PathBuf>,
    session_allow: Mutex<Vec<Rule>>,
    /// The active mode preset (L10.7): (name, read-only, only these tools).
    overlay: Mutex<Option<Overlay>>,
    /// Risk-gated commands the user approved for this session, verbatim.
    session_commands: Mutex<HashSet<String>>,
}

fn rules(list: &[String]) -> Vec<Rule> {
    list.iter()
        .filter_map(|s| {
            let r = Rule::parse(s);
            if r.is_none() {
                tracing::warn!(rule = %s, "ignoring malformed permission rule");
            }
            r
        })
        .collect()
}

impl Policy {
    pub fn new(perms: Permissions, home: PathBuf, global_settings: Option<PathBuf>) -> Self {
        Self {
            allow: rules(&perms.allow),
            ask: rules(&perms.ask),
            deny: rules(&perms.deny),
            perms,
            profile: Profile::default(),
            home,
            global_settings,
            session_allow: Mutex::default(),
            overlay: Mutex::default(),
            session_commands: Mutex::default(),
        }
    }

    /// Restrict to a named profile (subagents, `--profile`). Unknown names are an error.
    pub fn with_profile(mut self, name: &str) -> Result<Self, String> {
        let p = builtin_profile(name)
            .or_else(|| self.perms.profiles.get(name).cloned())
            .ok_or_else(|| format!("unknown profile `{name}`"))?;
        self.profile = p;
        Ok(self)
    }

    /// Only these tools may run (a subagent definition's `tools`, L10.1).
    /// Apply a mode preset's rules on top of the session's (`None` clears it).
    /// "Allow for this session" answers so far (`/reload` keeps them).
    pub fn session_rules(&self) -> Vec<Rule> {
        self.session_allow.lock().unwrap().clone()
    }

    pub fn allow_for_session(&self, rules: Vec<Rule>) {
        self.session_allow.lock().unwrap().extend(rules);
    }

    pub fn set_overlay(&self, o: Option<Overlay>) {
        *self.overlay.lock().unwrap() = o;
    }

    pub fn with_tools(mut self, tools: Vec<String>) -> Self {
        self.profile.tools = Some(tools);
        self
    }

    pub fn with_mode(mut self, mode: Mode) -> Self {
        self.perms.mode = mode;
        self
    }

    pub fn mode(&self) -> Mode {
        self.perms.mode
    }

    /// The mode a child agent starts with: read-only while a read-only preset
    /// (plan mode) is on, else the session's own.
    pub fn effective_mode(&self) -> Mode {
        if self.overlay.lock().unwrap().as_ref().is_some_and(|o| o.1) {
            Mode::ReadOnly
        } else {
            self.perms.mode
        }
    }

    /// The active preset's tool list, when it limits tools.
    pub fn overlay_tools(&self) -> Option<Vec<String>> {
        self.overlay.lock().unwrap().as_ref().and_then(|o| o.2.clone())
    }

    /// `interactive` = an approver can ask the user right now.
    pub fn check(&self, tool: &str, args: &Value, cwd: &Path, interactive: bool) -> Verdict {
        self.check_tool(tool, false, args, cwd, interactive, &HookDecision::None)
    }

    /// [`Policy::check`] for a tool that may declare it only reads (MCP
    /// `readOnlyHint`, extension `readOnly`), so it runs in read-only mode;
    /// `hook` = what `PreToolUse` hooks answered.
    pub fn check_tool(
        &self,
        tool: &str,
        declared_read_only: bool,
        args: &Value,
        cwd: &Path,
        interactive: bool,
        hook: &HookDecision,
    ) -> Verdict {
        match (
            self.verdict(tool, declared_read_only, args, cwd, interactive, hook),
            hook,
        ) {
            // A hook's ask only adds a question; it never weakens a deny. Asked
            // every time (the hook decides), so no "always allow" rule is offered.
            (Verdict::Allow, HookDecision::Ask(reason)) => Verdict::Ask {
                reason: reason.clone(),
                rule: suggest_rule(
                    tool,
                    (tool.eq_ignore_ascii_case("bash"))
                        .then(|| args.get("command").and_then(Value::as_str).unwrap_or("")),
                    args.get("path").and_then(Value::as_str),
                ),
                risky: true,
            },
            (v, _) => v,
        }
    }

    fn verdict(
        &self,
        tool: &str,
        declared_read_only: bool,
        args: &Value,
        cwd: &Path,
        interactive: bool,
        hook: &HookDecision,
    ) -> Verdict {
        let lower = tool.to_lowercase();
        // `process` start runs a shell command: the same rules and checks as `bash`.
        let start = lower == "process" && args.get("action").and_then(Value::as_str) == Some("start");
        let rule_tool = if start { "bash" } else { tool };
        if let Some(only) = &self.profile.tools
            && !only.iter().any(|t| t.eq_ignore_ascii_case(tool))
        {
            return Verdict::Deny(format!("The `{tool}` tool is not available to this agent."));
        }
        if self.profile.deny.iter().any(|t| t.eq_ignore_ascii_case(tool)) {
            return Verdict::Deny(format!("The `{tool}` tool is not available to this agent."));
        }
        let overlay = self.overlay.lock().unwrap().clone();
        if let Some((name, _, Some(only))) = &overlay
            && !only.iter().any(|t| t.eq_ignore_ascii_case(tool))
        {
            return Verdict::Deny(format!("The `{tool}` tool is not available in {name} mode."));
        }
        let read_only = self.perms.mode == Mode::ReadOnly || overlay.as_ref().is_some_and(|o| o.1);
        let mode_name = overlay.map_or("Read-only".to_string(), |o| {
            format!("{} mode is read-only", capitalise(&o.0))
        });

        let command = (lower == "bash" || start).then(|| args.get("command").and_then(Value::as_str).unwrap_or(""));
        let segments = command.map(|c| risk::segments(c).unwrap_or_else(|| vec![c.to_string()]));
        let raw_path = args.get("path").and_then(Value::as_str);
        let abs_path = raw_path.map(|p| expand(p, cwd, &self.home));
        let matches = |r: &Rule, all: bool| -> bool {
            if !r.matches_tool(rule_tool) {
                return false;
            }
            match &segments {
                Some(segs) if all => !segs.is_empty() && segs.iter().all(|s| r.matches_command(s)),
                Some(segs) => segs.iter().any(|s| r.matches_command(s)),
                None if FILE_TOOLS.contains(&lower.as_str()) => r.matches_path(raw_path, abs_path.as_deref()),
                None => r.pattern.is_none(),
            }
        };

        if let Some(r) = self.deny.iter().find(|r| matches(r, false)) {
            return Verdict::Deny(format!("Denied by the permission rule `{r}`."));
        }

        let mut read_only_bash = false;
        if let (Some(command), Some(segs)) = (command, &segments) {
            for s in segs {
                let name = s.split_whitespace().next().unwrap_or("");
                let name = name.rsplit('/').next().unwrap_or(name);
                if self.perms.protected.commands.iter().any(|c| c == name)
                    || s.split_whitespace()
                        .any(|w| self.perms.protected.commands.iter().any(|c| c == w))
                {
                    return Verdict::Deny(format!(
                        "`{name}` is a protected command in your settings and never runs."
                    ));
                }
            }
            let ctx = risk::Ctx {
                cwd,
                home: &self.home,
                protected: &self.perms.protected.paths,
            };
            let r = risk::classify(command, &ctx);
            match r.level {
                Level::Catastrophic => {
                    return Verdict::Deny(format!(
                        "Refused by the safety check: this command {}. It can never run from novacode. Do it another way, or ask the user to run it themselves.",
                        r.reason.trim_start_matches("rm ")
                    ));
                }
                // Read-only: refused below, never offered for approval (revision 2026-10-23).
                Level::Confirm | Level::Shared if read_only => {}
                // Auto mode never asks, like pi (user choice, revision L6.2): only the
                // catastrophic list, `Shared` (remote history, revision 2026-10-19) and the
                // user's own ask/deny rules stop a command.
                Level::Confirm if self.perms.mode == Mode::Auto => {
                    tracing::info!(command, reason = %r.reason, "risky command run in auto mode");
                }
                Level::Confirm | Level::Shared => {
                    if interactive {
                        // The user already approved this exact command; don't ask twice.
                        if self.session_commands.lock().unwrap().contains(command) {
                            return Verdict::Allow;
                        }
                        return Verdict::Ask {
                            reason: r.reason,
                            rule: command.to_string(),
                            risky: true,
                        };
                    } else {
                        let why = args.get("justification").and_then(Value::as_str).unwrap_or("").trim();
                        if why.len() < MIN_JUSTIFICATION {
                            return Verdict::Deny(format!(
                                "Held by the safety check: {}. Nobody is here to approve it. If it is really needed, call bash again with the same command and a `justification` field saying why it is safe. Otherwise do it a safer way.",
                                r.reason
                            ));
                        }
                        tracing::info!(
                            command,
                            justification = why,
                            "risk-gated command run with justification"
                        );
                    }
                }
                Level::Safe => read_only_bash = true,
                Level::Low => {}
            }
        }

        if read_only {
            let process_input = lower == "process" && args.get("action").and_then(Value::as_str) == Some("write");
            if MUTATING.contains(&lower.as_str())
                || matches!(lower.as_str(), "enter_worktree" | "exit_worktree")
                || process_input
            {
                return Verdict::Deny(format!("{mode_name}: `{tool}` can't change files now."));
            }
            if command.is_some() && !read_only_bash {
                return Verdict::Deny(format!(
                    "{mode_name}: only read-only shell commands run now (ls, cat, grep, git status, …)."
                ));
            }
            // An MCP, extension or client tool that doesn't say it only reads.
            if command.is_none() && !declared_read_only && !READ_TOOLS.contains(&lower.as_str()) {
                let allowed = self
                    .allow
                    .iter()
                    .chain(self.session_allow.lock().unwrap().iter())
                    .any(|r| matches(r, true));
                if allowed {
                    return Verdict::Allow;
                }
                let why = format!("{mode_name}: `{tool}` may change things (it doesn't declare itself read-only)");
                return if interactive {
                    Verdict::Ask {
                        reason: why,
                        rule: suggest_rule(tool, None, None),
                        risky: false,
                    }
                } else {
                    Verdict::Deny(format!("{why}, and nobody can approve it here."))
                };
            }
        }

        let session = self.session_allow.lock().unwrap();
        if self.allow.iter().chain(session.iter()).any(|r| matches(r, true)) {
            return Verdict::Allow;
        }
        drop(session);
        // Placed after deny rules, the risk gate and read-only mode, so a hook's allow skips only the asks below.
        if *hook == HookDecision::Allow {
            return Verdict::Allow;
        }

        let ask = |reason: String| Verdict::Ask {
            reason,
            rule: suggest_rule(rule_tool, command, raw_path),
            risky: false,
        };
        if let Some(r) = self.ask.iter().find(|r| matches(r, false)) {
            return ask(format!("matches the ask rule `{r}`"));
        }
        if self.perms.mode == Mode::Ask
            && (MUTATING.contains(&lower.as_str()) || (command.is_some() && !read_only_bash))
        {
            return ask(format!("ask mode: `{tool}` changes things"));
        }
        Verdict::Allow
    }

    /// Remember an approval for the rest of the session, or in global settings.
    pub fn record(&self, req: &ApprovalRequest, a: &Approval) {
        match a {
            Approval::Session | Approval::Always if req.risky => {
                self.session_commands.lock().unwrap().insert(req.rule.clone());
            }
            Approval::Session => self.session_allow.lock().unwrap().extend(Rule::parse(&req.rule)),
            Approval::Always => {
                self.session_allow.lock().unwrap().extend(Rule::parse(&req.rule));
                if let Some(path) = &self.global_settings {
                    let mut list = self.perms.allow.clone();
                    if !list.contains(&req.rule) {
                        list.push(req.rule.clone());
                    }
                    if let Err(e) = settings::set(path, "permissions.allow", &Value::from(list)) {
                        tracing::warn!(error = %e, "could not save the allow rule");
                    }
                }
            }
            Approval::Once | Approval::Deny(_) => {}
        }
    }
}

/// Built-in profiles; settings can define more under `permissions.profiles`.
fn builtin_profile(name: &str) -> Option<Profile> {
    let tools = |t: &[&str]| Profile {
        tools: Some(t.iter().map(|s| s.to_string()).collect()),
        deny: Vec::new(),
    };
    match name {
        "scout" | "reviewer" => Some(tools(&["read", "grep", "find", "ls"])),
        _ => None,
    }
}

fn capitalise(s: &str) -> String {
    let mut c = s.chars();
    c.next().map_or(String::new(), |f| f.to_uppercase().chain(c).collect())
}

/// What "always allow" would save: `Bash(<first two words>:*)` or `Edit(<path>)`.
fn suggest_rule(tool: &str, command: Option<&str>, path: Option<&str>) -> String {
    let mut name = tool.to_lowercase();
    if let Some(c) = name.get_mut(0..1) {
        c.make_ascii_uppercase();
    }
    match (command, path) {
        (Some(c), _) => {
            let words: Vec<&str> = c.split_whitespace().take(2).collect();
            format!("{name}({}:*)", words.join(" "))
        }
        (None, Some(p)) => format!("{name}({p})"),
        _ => name,
    }
}

fn expand(p: &str, cwd: &Path, home: &Path) -> PathBuf {
    match p.strip_prefix("~/") {
        Some(rest) => home.join(rest),
        None => cwd.join(p),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn policy(v: Value) -> Policy {
        Policy::new(serde_json::from_value(v).unwrap(), "/home/u".into(), None)
    }
    fn bash(c: &str) -> Value {
        json!({ "command": c })
    }
    const CWD: &str = "/home/u/proj";
    fn check(p: &Policy, tool: &str, args: Value, interactive: bool) -> Verdict {
        p.check(tool, &args, Path::new(CWD), interactive)
    }
    fn denied(v: &Verdict) -> bool {
        matches!(v, Verdict::Deny(_))
    }

    #[test]
    fn auto_mode_and_the_risk_gate() {
        let auto = policy(json!({}));
        // Auto mode: risky-but-ordinary commands run without asking; catastrophic ones never do.
        assert_eq!(check(&auto, "bash", bash("rm -rf target"), true), Verdict::Allow);
        assert_eq!(check(&auto, "bash", bash("git reset --hard"), false), Verdict::Allow);
        assert!(denied(&check(&auto, "bash", bash("rm -rf ~"), false)));
        // Remote history can't be checkpointed: asks even in auto mode, needs a justification headless.
        assert!(matches!(
            check(&auto, "bash", bash("git push --force origin HEAD:main"), true),
            Verdict::Ask { risky: true, .. }
        ));
        assert!(denied(&check(&auto, "bash", bash("git push -f"), false)));
        // The user's own ask rules still ask.
        let rule = policy(json!({ "ask": ["Bash(rm:*)"] }));
        assert!(matches!(
            check(&rule, "bash", bash("rm -rf target"), true),
            Verdict::Ask { .. }
        ));

        let p = policy(json!({ "allow": ["Bash(rm:*)"] })).with_mode(Mode::Ask);
        // An allow rule can't unlock a catastrophic command.
        assert!(denied(&check(&p, "bash", bash("rm -rf ~"), true)));
        // Ask mode, Confirm: asks a present user, needs a justification headless.
        assert!(matches!(
            check(&p, "bash", bash("rm -rf target"), true),
            Verdict::Ask { risky: true, .. }
        ));
        assert!(denied(&check(&p, "bash", bash("rm -rf target"), false)));
        let justified = json!({ "command": "rm -rf target", "justification": "stale build output, rebuilt next" });
        assert_eq!(check(&p, "bash", justified, false), Verdict::Allow);
        let blind = json!({ "command": "rm -rf target", "justification": "ok" });
        assert!(denied(&check(&p, "bash", blind, false)));
    }

    #[test]
    fn rules_profiles_and_modes() {
        let p = policy(json!({
            "deny": ["Bash(git push:*)", "Read(**/.env)"],
            "ask": ["Edit(src/**)"],
            "protected": { "commands": ["sudo"] },
        }));
        assert!(denied(&check(
            &p,
            "bash",
            bash("git add . && git push origin main"),
            false
        )));
        assert!(denied(&check(&p, "Bash", bash("ls; sudo ls"), true)));
        assert!(denied(&check(&p, "read", json!({"path": "a/.env"}), false)));
        assert!(matches!(
            check(&p, "edit", json!({"path": "src/main.rs"}), true),
            Verdict::Ask { .. }
        ));
        assert_eq!(check(&p, "edit", json!({"path": "docs/x.md"}), true), Verdict::Allow);

        let ro = policy(json!({})).with_mode(Mode::ReadOnly);
        assert!(denied(&check(&ro, "write", json!({"path": "a"}), true)));
        assert_eq!(check(&ro, "bash", bash("git status && ls"), true), Verdict::Allow);
        assert!(denied(&check(&ro, "bash", bash("cargo build"), true)));

        let ask = policy(json!({ "allow": ["Bash(cargo test:*)"] })).with_mode(Mode::Ask);
        assert_eq!(check(&ask, "bash", bash("cargo test -p x"), true), Verdict::Allow);
        assert_eq!(check(&ask, "read", json!({"path": "a"}), true), Verdict::Allow);
        // Compound: every part must be allowed.
        let Verdict::Ask { rule, .. } = check(&ask, "bash", bash("cargo test && cargo build --release"), true) else {
            panic!()
        };
        assert_eq!(rule, "Bash(cargo test:*)");

        let scout = policy(json!({})).with_profile("scout").unwrap();
        assert!(denied(&check(&scout, "bash", bash("ls"), true)));
        assert_eq!(check(&scout, "grep", json!({"pattern": "x"}), true), Verdict::Allow);
        assert!(policy(json!({})).with_profile("nope").is_err());
    }

    #[test]
    fn read_only_holds_on_every_path() {
        // `process` start is checked as its command, rules included.
        let deny = policy(json!({ "deny": ["Bash(curl:*)"] }));
        let start = |c: &str| json!({"action": "start", "command": c});
        assert!(denied(&check(&deny, "process", start("curl example.invalid"), true)));
        assert_eq!(check(&deny, "process", start("npm run dev"), true), Verdict::Allow);

        let plan = policy(json!({}));
        plan.set_overlay(Some(("plan".into(), true, None)));
        for p in [&policy(json!({})).with_mode(Mode::ReadOnly), &plan] {
            assert!(denied(&check(p, "process", start("touch x"), true)));
            assert_eq!(check(p, "process", start("git status"), false), Verdict::Allow);
            assert!(denied(&check(
                p,
                "process",
                json!({"action": "write", "id": "proc_1", "input": "y"}),
                true
            )));
            assert_eq!(
                check(p, "process", json!({"action": "output", "id": "proc_1"}), false),
                Verdict::Allow
            );
            assert!(denied(&check(p, "enter_worktree", json!({"name": "x"}), true)));
            assert!(denied(&check(p, "exit_worktree", json!({}), true)));
            // A risky command is refused, not offered for approval.
            assert!(denied(&check(p, "bash", bash("rm -rf build"), true)));
            // Children inherit the mode, so spawning stays allowed.
            assert_eq!(check(p, "agent", json!({"action": "spawn"}), false), Verdict::Allow);
            // Unknown effect: asks when someone can answer, refused otherwise, runs when declared read-only.
            assert!(denied(&check(p, "gh__create_issue", json!({}), false)));
            assert!(matches!(
                check(p, "gh__create_issue", json!({}), true),
                Verdict::Ask { risky: false, .. }
            ));
            assert_eq!(
                p.check_tool(
                    "gh__get_issue",
                    true,
                    &json!({}),
                    Path::new(CWD),
                    false,
                    &HookDecision::None
                ),
                Verdict::Allow
            );
        }
        // "Allow for this session" on an unknown tool holds in read-only mode.
        plan.allow_for_session(vec![Rule::parse("Gh__create_issue").unwrap()]);
        assert_eq!(check(&plan, "gh__create_issue", json!({}), false), Verdict::Allow);
        assert_eq!(plan.effective_mode(), Mode::ReadOnly);
        plan.set_overlay(Some(("docs".into(), false, Some(vec!["read".into()]))));
        assert_eq!(
            (plan.effective_mode(), plan.overlay_tools()),
            (Mode::Auto, Some(vec!["read".to_string()]))
        );
    }

    #[test]
    fn hook_decisions() {
        let hook = |p: &Policy, tool: &str, args: Value, h: HookDecision| {
            p.check_tool(tool, false, &args, Path::new(CWD), true, &h)
        };
        let allow = || HookDecision::Allow;
        let p = policy(json!({ "ask": ["Edit(src/**)"], "deny": ["Bash(git push:*)"] })).with_mode(Mode::Ask);
        // Allow skips the ask rule and ask mode…
        assert_eq!(hook(&p, "edit", json!({"path": "src/a.rs"}), allow()), Verdict::Allow);
        assert_eq!(hook(&p, "bash", bash("cargo build"), allow()), Verdict::Allow);
        // …but not deny rules, the risk gate or read-only mode.
        assert!(denied(&hook(&p, "bash", bash("git push origin"), allow())));
        assert!(denied(&hook(&p, "bash", bash("rm -rf ~"), allow())));
        assert!(matches!(
            hook(&p, "bash", bash("rm -rf target"), allow()),
            Verdict::Ask { risky: true, .. }
        ));
        let ro = policy(json!({})).with_mode(Mode::ReadOnly);
        assert!(denied(&hook(&ro, "write", json!({"path": "a"}), allow())));
        // Ask turns an allow into an ask, and never weakens a deny.
        let auto = policy(json!({ "deny": ["Read(**/.env)"] }));
        let ask = || HookDecision::Ask("plugin says check".into());
        assert_eq!(
            hook(&auto, "read", json!({"path": "a"}), ask()),
            Verdict::Ask {
                reason: "plugin says check".into(),
                rule: "Read(a)".into(),
                // Asked every time, so no "always allow" is offered.
                risky: true
            }
        );
        assert!(denied(&hook(&auto, "read", json!({"path": "x/.env"}), ask())));
        // Read-only mode isn't skipped by a hook's allow, for unknown tools either.
        assert!(matches!(
            hook(&ro, "gh__create_issue", json!({}), allow()),
            Verdict::Ask { .. }
        ));
        assert_eq!(allow().merge(ask()), ask());
        assert_eq!(HookDecision::None.merge(allow()), allow());
    }

    #[test]
    fn approvals_are_remembered() {
        let p = policy(json!({})).with_mode(Mode::Ask);
        let Verdict::Ask { rule, reason, risky } = check(&p, "bash", bash("npm run build"), true) else {
            panic!()
        };
        let req = ApprovalRequest {
            tool: "bash".into(),
            args: bash("npm run build"),
            reason,
            rule,
            risky,
        };
        p.record(&req, &Approval::Session);
        assert_eq!(check(&p, "bash", bash("npm run build -- --prod"), true), Verdict::Allow);

        let Verdict::Ask { rule, reason, risky } = check(&p, "bash", bash("git reset --hard"), true) else {
            panic!()
        };
        let req = ApprovalRequest {
            tool: "bash".into(),
            args: bash("git reset --hard"),
            reason,
            rule,
            risky,
        };
        p.record(&req, &Approval::Always);
        assert_eq!(check(&p, "bash", bash("git reset --hard"), true), Verdict::Allow);
        // Only that exact command, not a broad rule.
        assert!(matches!(
            check(&p, "bash", bash("git reset --hard HEAD~5"), true),
            Verdict::Ask { .. }
        ));
    }

    #[test]
    fn always_saves_to_global_settings() {
        let d = tempfile::tempdir().unwrap();
        let g = d.path().join("settings.json");
        std::fs::write(&g, "{\n  // keep me\n  \"permissions\": { \"allow\": [\"Read\"] }\n}\n").unwrap();
        let p = Policy::new(
            serde_json::from_value(json!({ "allow": ["Read"] })).unwrap(),
            "/home/u".into(),
            Some(g.clone()),
        );
        let req = ApprovalRequest {
            tool: "bash".into(),
            args: bash("make all"),
            reason: String::new(),
            rule: "Bash(make all:*)".into(),
            risky: false,
        };
        p.record(&req, &Approval::Always);
        let text = std::fs::read_to_string(&g).unwrap();
        assert!(
            text.contains("// keep me") && text.contains("Bash(make all:*)"),
            "{text}"
        );
    }
}
