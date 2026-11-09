# novacode-core/src

## Overview

The source of the `novacode-core` crate: the session owner task and the SDK that frontends embed. It holds the agent loop (L3.1), the tool pipeline (L3.2), permissions and the risk gate (L6), session storage and compaction memory (L4), and extension points: hooks, script and process extensions, MCP, packages and Claude Code plugins (L7). `lib.rs` only declares the 30 public modules below; it re-exports nothing.

## Interface

Loop and session:

- `agent.rs`: `Agent` (builder: `new`, `with_policy`, `with_hooks`, `with_mcp`, `with_extensions`, `with_memory`, `with_checkpoints`, `with_modes`, `with_approver`, `with_judge`, …, `run`, `compact`), `spawn(Agent) -> (SessionHandle, JoinHandle<Agent>)`, `SessionHandle` (`prompt`, `steer`, `remind`, `abort`, `subscribe`, `set_model`, `set_thinking`, `add_tools`, `side_question`, `rename`, `move_to`, `dequeue`, `compact`, `shutdown`, `prompts`, `context`, `prompt_content`, `steer_content`, `remind_as`), `Config`, `Switch`, `Carry`, `ContextReport`, `StateSaver`, `EVENT_BUFFER`.
- `session.rs`: `Store`, `StoreError`, `Header`, `Entry`, `EntryKind`, `PromptNode`, `Summary`, `VERSION`, `dir_for`, `list`, `find`, `summary`, `recent_prompts`, `cleanup_old`, `cleanup_daily`, `now_ms`.
- `pipeline.rs`: `Prepared`, `prepare`, `def`, `schema`, `authorize`, `authorize_cancellable`, `run`.
- `prompt.rs`: `Sections`, `Sources`, `EnvState`, `build`, `render`, `diff`, `render_update`, `context_files`, `global_file`, `env_state`, `environment`, `reminder`, `DEFAULT_PREAMBLE`.
- `context_window.rs`: `Builder`, `Plain`, `Quiet`, `StateFn`, `rebuild`.
- `recovery.rs`: `Recovery`, `Nudge`.
- `cache.rs`: `Sent`, `Miss`, `miss`, `human`, `warm_delay`, `return_chance`, `warm_saving`, `WARM_THRESHOLD`.
- `checkpoint.rs`: `Repo`, `Snapshot`, `Rewound`, `resolve`, `snapshot`, `pre_image`, `rewind`, `undo`, `session_base`, `MAX_UNTRACKED`.
- `memory.rs`: observer/reflector `Worker`, `Settings`, `Note`, `Relevance`, `Chunk`, `Reflection`, `Recall`, `observe`, `reflect`, `drop_notes`, `summarize`, `name_session`, `render_summary`, `cut_point`, prompt constants.
- `memstore.rs`: `Memories`, `MemoryTool`, `Scope`, `Item`.
- `export.rs`: `read`, `branch`, `jsonl`, `html`, `search`, `Hit`, `SessionSearch` tool.
- `metrics.rs`: `Record`, `record`, `load`, `path`, `hit_pct`.

Safety:

- `policy.rs`: `Policy`, `Mode` (`Auto`, `Ask`, `ReadOnly`), `Security`, `Permissions`, `Protected`, `Profile`, `Rule`, `Verdict`, `HookDecision`, `ApprovalRequest`, `Approval`, trait `Approver`, `Overlay`, `EditRule` (+ `ENV`).
- `risk.rs`: `classify`, `Risk`, `Level` (`Safe`, `Low`, `Confirm`, `Shared`, `Catastrophic`), `Ctx`, `segments`, `nested`, `programs`, `credential_dirs`, `protected_path`.
- `judge.rs`: `Judge`, `Decision`, `Answer`, `Subject`, `Seen`, `needs_judge`, `approves`, `parse`, `prompt`, `TIMEOUT`, `MAX_COMMAND`.
- `trust.rs`: `TrustStore`, `State`, `Fingerprint`, `resolve`, `fingerprint`, `has_project_config`, `path_key`, `fnv_hex`.

Tools and modes:

- `codemode.rs`: `Codemode` tool, `Runner`, `WaitClock`, `WaitGuard`, `NAME`.
- `classifier.rs`: `Classify` tool.
- `skill.rs`: `SkillTool`, `Fork`, `rule`.
- `web.rs`: `WebFetch`, `WebSearch`, `ENGINES`, `to_markdown`, `read_grounded`.
- `modes.rs`: `Modes`, `Preset`, `EnterPlanMode`, `PlanComplete`, `PLAN_PROMPT`.
- `resources.rs`: `Skill`, `Template`, `Seen`, `skills`, `templates`, `render_skills`, `expand`, `frontmatter`, `parse_frontmatter`.

Extension points:

- `hooks.rs`: trait `Handler`, `Hooks`, `Gate`, `Commands`, `Config`, `Run`, `After`, `Started`, `OneOrMany`, `spawn_observers`, `flush`, `disabled`, `DISABLED_ENV`, `writes_files`.
- `extensions.rs`: `Extensions`, `Loaded`, `Host`, `SessionApi`, `Sink`, `Reminder`, `discover`.
- `process_ext.rs`: `ProcessExt`, `Config` (JSON-RPC 2.0 over stdin/stdout).
- `mcp.rs`: `Hub`, `McpTool`, `ServerConfig`, `ToolOverride`, `Configured`, `ToolSpec`, `Status`, `load`, `project_file`, `project_dirs`, `expand`, `exposure`, `fingerprint`, `cached_signatures`, `write_signatures`, `edit_global`.
- `mcp_oauth.rs`: `OAuthConfig`, `Auth`, `Saved`, `Pending`, `begin`, `finish`, `remove`, `store_path`.
- `packages.rs`: `Store` (`install`, `update`, `uninstall`, `check`), `Package`, `Pin`, `Want`, `Available`, `latest`, `enabled`, `problems`, `declared`, `missing`, `install_project`, `version_of`, `name_of`.
- `plugin.rs`: `Kind`, `Parts`, `Hook`, `Servers`, `EVENTS`, `parse_hooks`, `fill_for`, `owner_name`.
- `plugin_hooks.rs`: `PluginHooks`, `TOOLS`, `claude_tool`.

## Invariants

- The agent loop never fails: errors end the run as an `AgentEnd` with a stop reason, and every effect is an appended session entry plus an event (`agent.rs`).
- Every tool call, from the model, codemode scripts, extensions or the classifier, goes through `pipeline.rs`, so permissions, hooks, and output guards and redaction cannot be skipped.
- Policy order (`policy.rs`): profile → turn `disallowed-tools` and mode-overlay tool list → deny rules → protected commands → risk gate → protected paths and task edit rule (file tools) → read-only mode → ask for writes to novacode settings, global config and shell startup files → allow rules → hook allow → ask rules → ask mode → allow. A hook ask turns an allow into an ask. Catastrophic and protected commands are refused before any allow rule is checked.
- The risk gate is biased toward recall: a command it cannot parse is escalated, never waved through. `Shared` asks in auto mode only at the `Hardened` security level; at the default `Lenient` level it runs and is logged. `Catastrophic` never runs. It is defense in depth, not a sandbox.
- The judge sees the user's messages, the user's `ask_user` answers and the agent's recent tool calls (name and arguments), never assistant text or tool output.
- Sessions are append-only JSONL trees with single-write appends. A torn last line is skipped on load, and the file is created lazily at the first real exchange. The projection is a pure function of the file.
- The system prompt prefix never changes after the first request. Changed sections and mode switches are appended as system deltas so the prompt cache holds (`prompt.rs`, `modes.rs`).
- Memory notes are session entries (`EntryKind::Notes`) and are never sent to the main model between compactions.
- Project executable config (settings, extensions, MCP, hooks, project skills) loads only for trusted folders. If its fingerprint changes, the folder counts as changed. AGENTS.md and CLAUDE.md always load.
- Checkpoints never touch the user's git index, HEAD or refs.
- A crashing or hung extension is disabled with a notice, and the session carries on.
- An MCP OAuth token is keyed by server name and URL, so it is never sent to a different server.

## Side Effects

- Writes session JSONL files, checkpoints under `<home>/checkpoints/`, memory SQLite databases under `<home>/memory/`, `<home>/trust.json`, `<home>/mcp-auth.json` (0600), `<home>/metrics/<session>.jsonl` (opt-in), MCP tool-list caches, and package lock files (`~/.novacode/packages.lock.json`, `.novacode/packages.lock.json`).
- Writes git objects into the repo's own object store through a temporary index (`checkpoint.rs`).
- Spawns child processes: shell hooks, Claude Code plugin hooks, process extensions, stdio MCP servers, and `git clone` for packages.
- Makes network calls: model providers (through `novacode-ai`), the judge, classifier and observer models, HTTP MCP servers and OAuth endpoints, and `web_fetch`/`web_search` (Google grounding, Gemini, Brave, SearXNG).
- Runs the tokio session owner task and broadcasts `AgentEvent`s to subscribers.

## Verification

- Run: `cargo test -p novacode-core`
