//! Every settings key novacode reads. `docs/config.md` is generated from
//! this table (a test fails when it is stale), and in debug builds
//! `Settings::get` panics on a key missing here, so the docs can't drift.

/// (key, type, default, what it does). `<name>` matches one path segment.
pub const KEYS: &[(&str, &str, &str, &str)] = &[
    (
        "compat.claude",
        "bool",
        "true",
        "Also load skills, agents and commands from `~/.claude` and a trusted project's `.claude`. novacode's own folders win a name. `~/.agents/skills` loads either way.",
    ),
    (
        "model",
        "string",
        "picked at first run",
        "Default `provider/model[:level]`, e.g. `claude/claude-opus-5-5:medium`.",
    ),
    (
        "thinking",
        "string",
        "`medium`",
        "Default thinking level: off, minimal, low, medium, high, xhigh, max.",
    ),
    (
        "maxTokens",
        "number",
        "16384",
        "Room for the answer per reply. Reasoning models get 16384 more for thinking. The total is clamped to the model's limit.",
    ),
    (
        "contextWindow",
        "number",
        "from the catalog",
        "Override the model's context window (normally a smaller one).",
    ),
    (
        "cacheRetention",
        "string",
        "`short`",
        "`long` asks Anthropic (Claude) for 1h prompt caching instead of 5 min. Other providers ignore it.",
    ),
    (
        "cache.warm",
        "bool",
        "false",
        "Keep the prompt cache alive with 1-token replays while it pays off.",
    ),
    (
        "providers",
        "object",
        "{}",
        "Add a provider or change a bundled one: `providers.<id>.{api, name, baseUrl, auth, env, apiKey, keyOptional, catalog, headers, compat}`, merged field by field over the bundled profile. See docs/providers.md.",
    ),
    (
        "models",
        "object",
        "{}",
        "Per-model catalog overrides, keyed `\"<provider>/<id>\"`: `contextWindow`, `maxTokens`, `images`, `reasoning` (bools), `name`, `cost`. See docs/providers.md.",
    ),
    (
        "models.deny",
        "list",
        "[]",
        "Models never used, as globs (`*` any run, `?` one character), e.g. `[\"claude-fable*\"]`. Each glob is matched against `provider/model`, the bare model id and its last `/` segment, after aliases resolve. A denied session model, `/model` or subagent model is an error; a denied helper, judge or resumed model falls back with a notice. Denied models leave the model list. Prices of past replies still count.",
    ),
    (
        "permissions",
        "object",
        "auto mode, lenient",
        "Global only. `security` lenient|hardened: hardened adds the command judge, the sandbox and asks for shared commands and config changes (docs/security.md#security-levels); `mode` auto|ask|read-only; `allow`/`ask`/`deny` lists of rules like `Bash(git push:*)`; `protected: {commands, paths}`; `profiles.<name>.{tools, deny}`, named tool sets; `judge`: the command judge's model in auto mode at the hardened level, `off` or a `provider/model[:level]` (default the observer's model). See docs/security.md.",
    ),
    (
        "tools.<name>.exposure",
        "string",
        "per tool",
        "How a tool reaches the model: `listed` (declared to it), `codemode` (alias `codemode-only`: only codemode scripts call it), `deferred` (found through `tool_search`) or `hidden` (user and UI only).",
    ),
    (
        "hooks",
        "object",
        "{}",
        "Command hooks: `beforeTool`, `afterTool` and `events.<type>`, each a command or a list; `timeoutMs` (default 5000) limits `beforeTool`, `afterTool` gets twice that. See docs/automation.md.",
    ),
    (
        "hooks.onActivity",
        "string",
        "unset",
        "Shell command run on working/blocked/idle changes, State JSON on stdin. Killed after 10 s.",
    ),
    (
        "processExtensions",
        "object",
        "{}",
        "Long-running JSON-RPC extensions in any language: `<name>: {command, args, env}`. See docs/extensions.md.",
    ),
    (
        "modes",
        "object",
        "{}",
        "Mode presets: `<name>: {prompt, readOnly, tools}`; `plan` is built in.",
    ),
    (
        "updates.check",
        "bool",
        "true",
        "At interactive start, show when novacode or an installed package has a newer release (checked at most once a day; nothing is installed). See docs/packages.md.",
    ),
    (
        "checkpoints.enabled",
        "bool",
        "true",
        "File checkpoints per prompt, for `/rewind`.",
    ),
    (
        "extensions.<name>",
        "object",
        "unset",
        "Settings of extension `<name>`: a process extension gets them in `initialize`, any extension through `novacode.settings.get(\"extensions.<name>\")`.",
    ),
    (
        "sandbox.mode",
        "string",
        "`auto` if hardened, else credentials only",
        "Global only. Run the model's `bash` commands and `process` starts in a bubblewrap sandbox: `auto` (when bwrap works; otherwise one warning, then unsandboxed), `on` (refuse commands when it can't sandbox) or `off`. Unset at the lenient level: a credentials-only layer (only credential folders, token files and `~/.novacode` read-only). See docs/security.md.",
    ),
    (
        "sandbox.allowWrite",
        "list",
        "[]",
        "Global only. More folders the sandbox lets commands write to (`~/` allowed), besides the project, `/tmp`, `$TMPDIR` and the tool caches.",
    ),
    (
        "sandbox.denyRead",
        "list",
        "[]",
        "Global only. More files and folders hidden inside the sandbox (`~/` allowed), besides credentials, logins, token files and private data (shell history, browser and mail profiles, the keyring).",
    ),
    (
        "security.maskSecrets",
        "bool",
        "false",
        "Also mask well-known secret shapes in tool output (GitHub/AWS/OpenAI/Anthropic/Slack/Google tokens, private keys, `*_TOKEN=`/`*_SECRET=` values) before the model sees them. Your own logins are always masked.",
    ),
    (
        "memory.enabled",
        "bool",
        "true",
        "Observer notes, compaction from notes and the `recall` tool. Off: compaction still runs, summarised by the session model.",
    ),
    (
        "memory.model",
        "string",
        "the profile's small model",
        "Model for the observer, reflector and dropper. Without a small model (or one that can't connect), the session's model.",
    ),
    (
        "memory.observeAfterTokens",
        "number",
        "10000",
        "New tokens before the observer runs.",
    ),
    (
        "memory.keepRecentTokens",
        "number",
        "20000",
        "Tokens kept verbatim after a compaction.",
    ),
    (
        "memory.compactAtTokens",
        "number",
        "min(80% of window, 400k)",
        "Context size that triggers auto-compaction. During a run it fires at 95% of the window at most; a value at or above the window turns auto-compaction off.",
    ),
    (
        "memory.reflectAfterTokens",
        "number",
        "20000",
        "New conversation tokens before the reflector runs.",
    ),
    (
        "memory.poolTargetTokens",
        "number",
        "10000",
        "Size the dropper trims active notes down to.",
    ),
    (
        "memory.store",
        "bool",
        "true",
        "Cross-session memory (SQLite) and its start-of-session index.",
    ),
    (
        "memory.indexItems",
        "number",
        "15",
        "Memory items in the session-start index.",
    ),
    ("memory.indexChars", "number", "2000", "Character cap of that index."),
    (
        "sessions.cleanupDays",
        "number",
        "14",
        "At start (at most once a day), delete subagent and super review sessions untouched this many days. User and super task sessions are kept. 0 keeps all.",
    ),
    (
        "agents.stallMinutes",
        "number",
        "10",
        "A subagent silent this long is stopped.",
    ),
    (
        "agents.timeoutMinutes",
        "number",
        "45",
        "A subagent running this long is stopped.",
    ),
    (
        "agents.maxConcurrent",
        "number",
        "4",
        "Subagents running at once; more wait as `queued`, in spawn order. A subagent's own subagents run one at a time.",
    ),
    (
        "agents.parentPicksTools",
        "bool",
        "true",
        "The model may pick a subagent's tools (`tools`) and MCP servers (`mcp`) when it spawns one, from its own. Off: only the type's tools.",
    ),
    (
        "agents.env",
        "object",
        "{}",
        "Environment variables every subagent starts with, e.g. `{\"CAVEMAN_DEFAULT_MODE\": \"off\", \"PONYTAIL_SUBAGENT_MATCHER\": \"^$\"}` keeps those plugins' `SubagentStart` text out of subagents.",
    ),
    (
        "agents.<name>.tools",
        "list",
        "the agent file's",
        "Default tools of subagent type `<name>` (built-ins too); a subagent sees only its tools.",
    ),
    (
        "agents.<name>.mcp",
        "list",
        "the agent file's",
        "MCP servers subagent type `<name>` gets by default (subagents start none otherwise).",
    ),
    (
        "worktree.copy",
        "list",
        "`[\"AGENTS.md\", \"CLAUDE.md\", \".mcp.json\", \".codegraph/\", \".claude/\", \".novacode/\"]`",
        "Untracked files and folders (relative to the repo root) copied into a new worktree (`-w`, `/worktree`, `enter_worktree`, subagent worktrees), besides root `.env*` files. Folders are copied whole except files git tracks; `daemon.*` files, sockets and folders holding their own git repo are never copied. `.codegraph/` is tool state: nothing untracked in it counts as a change, and it is deleted when the worktree is removed. In other copied folders, a file added or edited there counts as a change. A copied file counts as a change once it is edited or committed, and is then kept; unchanged copies are deleted only when git will then remove the worktree.",
    ),
    (
        "super.maxRunning",
        "number",
        "5",
        "Super mode (docs/super.md): tasks running at once, review tasks included; a new `task spawn` over the limit waits as `queued`. Messages to existing tasks never wait. At least 1: `0` counts as 1.",
    ),
    (
        "super.tiers",
        "object",
        "presets by model family: `claude` haiku/sonnet/opus (high); `codex` gpt-luna/gpt-sol/gpt-astra (high); `antigravity` gemini-flash medium/gemini-flash high/Gemini Argon (fallback: gemini-flash high)",
        "Super mode task models (docs/super.md#models). Tiers `quick`, `standard`, `deep` follow super's current provider; each preset is the newest model of a family that the provider offers, is logged in and not denied. `<provider>: {<tier>: \"provider/model[:level]\"}` overrides a tier on that provider. The older `<tier>: {model, note}` form overrides work tiers on every provider. Reviews use super's current exact provider/model and actual thinking level, ignoring legacy `review` and `deep` overrides; an explicit blocked-review retry resolves the current configuration again. On a provider without presets or overrides, every work tier uses super's own model.",
    ),
    (
        "super.tools",
        "list",
        "[]",
        "Extra tools super's model sees besides `task`, `ask_user`, `memory`, `todo` and `agent`. Scouts and tasks it starts get the regular tool set either way.",
    ),
    (
        "super.watch.everyMinutes",
        "number",
        "30",
        "Super mode: while a task runs, super gets a health-check wake this often and judges the tasks from code's health facts; a check that finds all fine is hidden from your window. Skipped while super works. `0`: no checks.",
    ),
    (
        "super.watch.progressMinutes",
        "number",
        "20",
        "Super mode: a task that has worked this long without a `progress` report is asked for one at its next tool boundary (no interrupt); never while it waits on your answer. `0`: no nudges.",
    ),
    (
        "super.watch.stallMinutes",
        "number",
        "15",
        "Super mode: a task blocked, stopped or ready but not landed, with no open question to you and no action of super's on it for this long, is stalled: it wakes a health check that lists it, and super must act on it. `0`: no stall checks.",
    ),
    (
        "super.watch.remindMinutes",
        "number",
        "30",
        "Super mode: a card waiting for your answer (a question, an approval, a card novacode made) is asked again this often, with a desktop notification and a short reminder naming the task and the decision, until you answer or dismiss it. Super's model is not woken. `0`: no reminders.",
    ),
    (
        "goal.audit",
        "bool",
        "true",
        "`/goal` completion is checked by the `auditor` subagent before it counts.",
    ),
    (
        "goal.auditModel",
        "string",
        "the session's model",
        "Model of the goal auditor (`provider/model[:level]`).",
    ),
    (
        "goal.maxRuns",
        "number",
        "30",
        "Automatic continuations of a goal before it pauses (per start or `/goal resume`).",
    ),
    (
        "goal.maxTokens",
        "number",
        "none",
        "Tokens (prompt + output) a goal may spend; then the agent summarises its progress and the goal pauses.",
    ),
    (
        "goal.judge",
        "string",
        "the observer's model",
        "Checks after each turn whether the goal is met: a `provider/model`, a classifier model (e.g. `typesafe/jev-latest`: calibrated probabilities, met needs 0.8), or `off` (only the agent's `complete` ends it).",
    ),
    (
        "web.native",
        "bool",
        "false",
        "Use the model's own web search when it has one.",
    ),
    (
        "web.search",
        "string",
        "auto",
        "`web_search` engine tried first: `auto`, `antigravity`, `gemini`, `brave` or `searxng`; the others stay as fallbacks. Set with `/websearch`.",
    ),
    (
        "web.searxngUrl",
        "string",
        "unset",
        "SearXNG instance for `web_search`, the last fallback.",
    ),
    (
        "debug.metrics",
        "bool",
        "false",
        "Per-session timing JSONL in `~/.novacode/metrics/` for `novacode stats`.",
    ),
    (
        "notifications.enabled",
        "bool",
        "true",
        "OSC 777 notification when a turn ends unwatched or needs you.",
    ),
    (
        "tmux.agentState",
        "bool",
        "true",
        "Set the `@agent_*` pane options inside tmux.",
    ),
    (
        "ui.theme",
        "string",
        "`auto`",
        "auto (follows the terminal background), dark (catppuccin-mocha), light (catppuccin-latte) or a file in `~/.novacode/themes` or an installed package's `themes` folder.",
    ),
    (
        "ui.turnCollapse",
        "bool",
        "true",
        "Fold finished turns to one line (Alt+O toggles).",
    ),
    (
        "ui.thinking",
        "string",
        "`collapsed`",
        "How thinking shows: collapsed, full or hidden.",
    ),
    (
        "ui.userMessage",
        "object",
        "rounded box",
        "Your messages' box: `border` (rounded, single, heavy, double, ascii, none), `borderColor` and `text` (`#rrggbb` or `{fg, bg, bold, italic, …}`), `paddingX` (1), `paddingY` (0).",
    ),
    (
        "ui.statusline",
        "object",
        "built-in rows",
        "`placement` (`above` the input, else below) and `rows` of segments (activity, ctx, cache, model, …). Segment names are in docs/tui.md.",
    ),
    (
        "ui.icons",
        "string",
        "`nerd`",
        "`unicode` when your font has no Nerd Font glyphs; any other value means `nerd` (`novacode doctor` flags it).",
    ),
    (
        "ui.images",
        "string",
        "`auto`",
        "Image protocol: auto, kitty, iterm2, sixel, halfblocks or off.",
    ),
    (
        "ui.panel.auto",
        "bool",
        "true",
        "Open the side panel by itself while todos or subagents run.",
    ),
    (
        "ui.panel.host",
        "string",
        "`inline`",
        "inline, tmux or auto: draw the panel in novacode or in a tmux pane. `tmux` and `auto` only take effect inside tmux.",
    ),
    ("ui.panel.tmuxWidth", "string", "`30%`", "Width of the tmux panel pane."),
    (
        "ui.slow",
        "bool",
        "on over SSH or when the load average is above the core count",
        "Lower frame rate and animations.",
    ),
];

/// Allowed values of the string keys that take one of a fixed set.
pub const CHOICES: &[(&str, &[&str])] = &[
    ("thinking", &["off", "minimal", "low", "medium", "high", "xhigh", "max"]),
    ("cacheRetention", &["short", "long"]),
    (
        "tools.<name>.exposure",
        &["listed", "codemode", "codemode-only", "deferred", "hidden"],
    ),
    ("sandbox.mode", &["auto", "on", "off"]),
    ("ui.thinking", &["collapsed", "full", "hidden"]),
    ("ui.icons", &["nerd", "unicode"]),
    ("ui.images", &["auto", "kitty", "iterm2", "sixel", "halfblocks", "off"]),
    ("ui.panel.host", &["inline", "tmux", "auto"]),
];

fn matches(pattern: &str, key: &str) -> bool {
    let (a, b): (Vec<_>, Vec<_>) = (pattern.split('.').collect(), key.split('.').collect());
    a.len() == b.len() && a.iter().zip(&b).all(|(x, y)| x.starts_with('<') || x == y)
}

/// Problems in a merged settings value: unknown keys, wrong types, values outside a
/// fixed set. Objects documented as a whole (`providers`, `hooks`, …) aren't looked into.
pub fn check(v: &serde_json::Value) -> Vec<String> {
    let mut out = Vec::new();
    walk(v, "", &mut out);
    out
}

fn walk(v: &serde_json::Value, path: &str, out: &mut Vec<String>) {
    use serde_json::Value;
    if !path.is_empty()
        && let Some((k, ty, ..)) = KEYS.iter().find(|(k, ..)| matches(k, path))
    {
        let ok = match *ty {
            "string" => v.is_string(),
            "number" => v.is_number(),
            "bool" => v.is_boolean(),
            "list" => v.is_array(),
            _ => v.is_object(),
        };
        if !ok {
            out.push(format!("`{path}`: expected a {ty}, got {v}; using the default"));
        } else if let (Some((_, allowed)), Some(s)) = (CHOICES.iter().find(|(c, _)| *c == *k), v.as_str())
            && !allowed.contains(&s)
        {
            out.push(format!(
                "`{path}`: `{s}` is not one of {}; using the default",
                allowed.join(", ")
            ));
        }
        return;
    }
    let prefix = |k: &str| {
        let n = if path.is_empty() { 0 } else { path.split('.').count() };
        let parts: Vec<_> = k.split('.').collect();
        parts.len() > n && matches(&parts[..n].join("."), path)
    };
    match v {
        Value::Object(m) if path.is_empty() || KEYS.iter().any(|(k, ..)| prefix(k)) => {
            for (k, child) in m {
                let p = if path.is_empty() {
                    k.clone()
                } else {
                    format!("{path}.{k}")
                };
                walk(child, &p, out);
            }
        }
        _ => out.push(format!("`{path}`: unknown setting (see docs/config.md)")),
    }
}

/// Whether `key` is in [`KEYS`]; a `<name>` segment matches any one segment.
pub fn documented(key: &str) -> bool {
    KEYS.iter().any(|(k, ..)| matches(k, key))
}

/// The Markdown reference in `docs/config.md`.
pub fn render() -> String {
    let mut s = String::from(
        "# Settings reference\n\n\
         <!-- Generated from crates/novacode-base/src/settings_doc.rs; edit there. -->\n\n\
         Global settings live in `~/.novacode/settings.json` and project settings in \
         `.novacode/settings.json` (read only when the folder is trusted). Both are JSON with \
         comments; the project wins key by key, except `permissions` and `sandbox`, which are global only. \
         Dotted keys are nested objects: `ui.theme` is `{\"ui\": {\"theme\": …}}`. A wrong value \
         is reported by `novacode doctor` and treated as unset.\n\n\
         | Key | Type | Default | What it does |\n|---|---|---|---|\n",
    );
    for (k, t, d, w) in KEYS {
        // A bare `|` in a cell would split the table row.
        let (d, w) = (d.replace('|', "\\|"), w.replace('|', "\\|"));
        s.push_str(&format!("| `{k}` | {t} | {d} | {w} |\n"));
    }
    s.push_str(
        "\nOther files in `~/.novacode`: logins and keys in `auth.json`, MCP servers in `mcp.json` \
         ([mcp](mcp.md)), trusted folders in `trust.json` ([security](security.md)). The full list is \
         in [install](install.md).\n\n\
         ## Environment variables\n\n\
         | Variable | What it does |\n|---|---|\n\
         | `NOVACODE_HOME` | Use this folder instead of `~/.novacode`. |\n\
         | `NOVACODE_LOG` | Log level or filter, e.g. `debug` ([troubleshooting](troubleshooting.md)). |\n\
         | `NOVACODE_HOOKS_DISABLED` | Any value turns command hooks off ([automation](automation.md)). |\n\
         | Provider keys (`ANTHROPIC_API_KEY`, `OPENROUTER_API_KEY`, …) | API keys, see [providers](providers.md). |\n\
         | `BRAVE_API_KEY`, `GEMINI_API_KEY` | Web search engines ([tools](tools.md)). |\n\
         | `VISUAL`, `EDITOR` | The editor `Ctrl+G` opens. Terminal variables are in [tui](tui.md). |\n",
    );
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn docs_match_the_table() {
        let file = concat!(env!("CARGO_MANIFEST_DIR"), "/../../docs/config.md");
        if std::env::var_os("UPDATE_SNAPSHOTS").is_some() {
            std::fs::write(file, render()).unwrap();
        }
        let on_disk = std::fs::read_to_string(file).unwrap_or_default();
        assert!(
            on_disk == render(),
            "docs/config.md is stale: run with UPDATE_SNAPSHOTS=1"
        );
        assert!(documented("tools.bash.exposure") && !documented("tools.bash"));
    }

    #[test]
    fn check_finds_unknown_keys_types_and_choices() {
        let v = serde_json::json!({
            "ui": {"icons": "bogus", "theme": "light", "panel": {"auto": "yes"}},
            "thinking": "super", "maxTokens": "x", "bogusKey": 1,
            "providers": {"x": {"apiKey": "k"}},
            "tools": {"bash": {"exposure": "codemode"}, "read": {"exposure": "nope"}},
            "memory": {"enabled": true, "typo": 1}
        });
        let p = check(&v);
        let has = |s: &str| p.iter().any(|m| m.contains(s));
        assert!(
            has("`ui.icons`: `bogus`") && has("`thinking`: `super`") && has("`maxTokens`: expected a number"),
            "{p:?}"
        );
        assert!(
            has("`ui.panel.auto`: expected a bool") && has("`bogusKey`: unknown") && has("`memory.typo`: unknown"),
            "{p:?}"
        );
        assert!(
            has("`tools.read.exposure`: `nope`") && !has("tools.bash") && !has("providers") && !has("ui.theme"),
            "{p:?}"
        );
        assert_eq!(p.len(), 7, "{p:?}");
        assert!(check(&serde_json::json!({"model": "claude/x", "ui": {"turnCollapse": false}})).is_empty());
        let agents = serde_json::json!({"agents": {"stallMinutes": 5, "parentPicksTools": false,
            "scout": {"tools": ["read"], "mcp": "codegraph"}}});
        assert_eq!(
            check(&agents),
            vec!["`agents.scout.mcp`: expected a list, got \"codegraph\"; using the default"]
        );
    }
}
