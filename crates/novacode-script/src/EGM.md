# novacode-script/src

## Overview
QuickJS runtime (via `rquickjs`) for codemode and script extensions (L7.2, L7.5), plus TypeScript stripping and pre-run checks with `oxc`. Codemode runs one model-written script in a fresh context; extensions keep one long-lived runtime each. Both reach the outside world only through a host trait the core implements.

## Interface
- `codemode.rs` (L7.5): `Host` trait (`call(name, args)`, `user_wait()`), `ToolInfo`, `Limits` (default 64 MiB memory, 300 s timeout, 500 calls, 16 MiB output), `Item` (text / console / image output), `Outcome` (incl. the whole `store` after the run; the `store()` key-value store is capped at 8 MiB of JSON, `STORE_CAP`), `declaration(&ToolInfo)` (TypeScript-ish declaration from a JSON schema), `async run(...)` (runs JS or TS as the body of an async function; `cancel` aborts it).
- `ext.rs` (L7.2 tier 1): `API_VERSION = 1`, `ExtHost` trait (`call(op, args)` for `exec`, `readFile`, `writeFile`, `notify`, `remind`, `log`, `ui`, `ask`, `messages`, `complete`, `spawnAgent`, `sendMessage`, `settings`, `stateAppend`, `stateEntries`), `ToolReg`, `CommandReg`, `Registration`, `Failure` (`Threw`, `Fatal`), `Extension` (`load(name, js, host, memory_bytes, imports: Option<(entry, cache)>)` (setup times out after 10 s), `invoke(handler, payload, timeout)`), `Backend` trait (shared with process extensions in the core).
- JS API v1 given to `export default function (novacode) {…}`: `registerTool`, `registerCommand`, `on(event, handler)` (`before_tool`, `after_tool`, `input`, `before_request`, session events), `exec`, `fs.read`, `fs.write`, `session.messages`, `complete`, `spawnAgent`, `sendMessage(text, {deliverAs})`, `settings.get`, `state.append`, `state.entries`, `notify`, `remind`, `log`, `apiVersion`, `registerShortcut(key, handler)`, and `ui` (`version`, `setStatus`, `setWidget`, `setFooter`, `setHeader`, `ask`, `select`, `confirm`, `input`, `setEditor`, `openTab`, `showTab`, `closeTab`, `setSource`, `publish`, `unpublish`, `setRegion`).
- `ts.rs`: `to_js(src, module)` (strip TS types), `cached(src, cache_dir)` (strip once per source hash + novacode version).
- `check.rs` (private module, L7.17): pre-run checks — `check(js, globals, tools)`, `validate(schema, args)`, `hint(err, code)`, `likely`, `lev`, `NODE_NAMES`.

## Invariants
- Codemode: a fresh QuickJS context per script, on its own thread. Every tool call goes through `Host::call`, which the core routes through the normal tool pipeline (permissions, hooks, guards); scripts get no other access.
- Time spent waiting on the user (`Host::user_wait`) does not count toward the codemode timeout.
- Pre-run checks deny by evidence only: when a check is unsure, the script passes.
- Extensions: one long-lived runtime per extension on its own thread; handlers are invoked by numeric id. After `Failure::Fatal` (deadline, out of memory, thread gone) the extension is not usable; after `Failure::Threw` it stays usable.
- Relative imports in an extension resolve only inside the entry file's folder (L7.10).
- `to_js` returns plain JS unchanged so error line numbers stay the model's own; unparsable input is returned unchanged so QuickJS reports the error.

## Side Effects
- Spawns one OS thread per codemode run and per loaded extension.
- `ts::cached` reads and writes stripped-module files in the cache directory.
- Extension imports: the resolver checks candidate files with `is_file` and `std::fs::canonicalize`, and the loader reads imported modules with `std::fs::read_to_string` (then strips them through `ts::cached`).
- All other I/O (exec, files, network, UI) happens in the host via `Host` / `ExtHost`, not in this crate.

## Verification
- Run: `cargo test -p novacode-script`
