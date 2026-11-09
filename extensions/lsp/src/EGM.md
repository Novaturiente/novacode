# novacode-lsp (src)

## Overview
Source of the `novacode-lsp` process extension (see `docs/extensions.md`). It listens to novacode's `after_tool` event and appends the errors an edit left in the touched files, as reported by real language servers. It talks to novacode over line-delimited JSON-RPC 2.0 on stdin/stdout; requests may overlap, so each is answered from its own tokio task.

- `main.rs`: the JSON-RPC loop and the `handle` dispatcher.
- `lsp.rs`: the language-server clients (LSP over `Content-Length` framing), config, and diagnostic diffing.

## Interface
- JSON-RPC `initialize` (params `settings` = `extensions.lsp` settings, `cwd`, `home`) → `{ "events": ["after_tool"], "timeoutMs": waitMs + 5000 }`. Bad settings fall back to `Config::default()` with a stderr warning.
- JSON-RPC `event` with `params.event == "after_tool"` and `payload { tool, isError, touched[] }` → `{ "append": "diagnostics: <text>" }` or `null` when there is nothing to say.
- JSON-RPC `shutdown` → exits the read loop. Notifications (no `id`) and unparseable lines are ignored.
- `pub struct Config { enabled, wait_ms, severity, max_items, servers }` (camelCase in settings; defaults `true`, `3000`, `"error"`, `10`, empty).
- `pub struct Server { command, args, extensions, root_markers }`.
- `pub fn servers(cfg: &Config) -> (Vec<(String, Server)>, Vec<String>)`: built-ins merged with settings, filtered to commands on `PATH`, plus warnings.
- `pub fn uri(p: &Path) -> String`: percent-encoded `file://` URI.
- `pub struct Diag { line, col, severity, message, source }`.
- `pub struct Lsp`: `new(cfg, cwd, logs) -> (Arc<Lsp>, warnings)`, `has_servers()`, `check(paths, baseline_only) -> Option<String>`, `after_tool(tool, is_error, touched) -> Option<String>`.

## Invariants
- Built-in servers: `rust-analyzer` (rs), `typescript-language-server --stdio` (ts/js family), `pyright-langserver --stdio` (py, pyi), `gopls` (go), `clangd` (C/C++). Settings `servers.<name>` add or replace one; `false` or `null` turns it off. A server whose command is not on `PATH` is dropped.
- One client per (server name, project root). The root is the nearest ancestor with a root marker, else the session `cwd`. Clients start lazily on first use and stay warm.
- Servers start in the background, one per (server, root), and are never retried after a failure (the error is cached). A server whose output closes fails its waiting requests at once (`the server closed during initialize`) instead of at the 30 s `initialize` timeout. The failure is logged to stderr and told to the model once, on the first non-`read` check; a `read` never uses it up. A check that reaches its deadline before the server is up reports `not checked, <name> is still starting`.
- Nothing is reported when the extension is disabled, there are no servers, or the tool call failed (`isError`).
- A `read` only warms the server and records the file's current diagnostics as the baseline; it returns nothing. Later edits report only what changed against what the model was last told.
- Other files are reported only when their problem count grew (a broken caller). A file that became clean is reported as `clean now`.
- Each check has one deadline, `waitMs` from its start, shared by every touched file and by waiting for a starting server, so it answers within the declared `timeoutMs` (`waitMs + 5000`); past it novacode disables the extension. All files are sent to their servers first, then each is waited for. After a file's publish, a 300 ms quiet spell catches follow-ups; other files' publishes don't extend it to the deadline. Diagnostics count only when the server is ready (no open `$/progress` tokens, rust-analyzer `quiescent` not false). If still loading, the file is reported as `not checked, <name> is still loading the project`, never as clean.
- When rust-analyzer reports itself unhealthy, its own (`source == "rust-analyzer"`) diagnostics are dropped and only `rustc` ones shown; the model is told once, on the first non-`read` check.
- `severity: "warning"` also lists warnings; at most `maxItems` items per file; messages are flattened to one line and cut at 240 chars.

## Side Effects
- Spawns language-server child processes (own process group, killed on drop) with `cwd` = project root.
- Disk: creates `<home>/logs/` (`NOVACODE_HOME` if set and non-empty, else `<params.home>/.novacode`, with `/tmp/.novacode` when `params.home` is missing) and writes server stderr to `lsp-<name>.log`; reads touched files.
- Sends `didOpen`/`didChange`/`didSave` to servers for touched files; answers server requests (`workspace/configuration`, `workspace/workspaceFolders`).
- Writes JSON-RPC answers to stdout and warnings to stderr.

## Verification
- Run: `cargo test --manifest-path extensions/lsp/Cargo.toml` (the rust-analyzer and pyright tests return early when those servers are not on `PATH`).
