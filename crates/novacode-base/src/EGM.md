# novacode-base/src

## Overview
Shared foundations (layer L0) used by the rest of novacode: home and project paths, crash-safe storage, layered JSONC settings, ids, logging, secret masking, hardened git commands and small OS-specific helpers.

## Interface
- `lib.rs`: `new_id() -> uuid::Uuid` (time-sortable UUIDv7, L0.8); modules below.
- `paths.rs` (L0.4): `Paths` (`new`, `from_env`, `home`, `settings`, `logs`, `project_dir(cwd)`, `project_settings(cwd)`), `NoHome`, `LOGIN_FILES` (`auth.json`, `mcp-auth.json`, `trust.json`), `CONFIG_ENTRIES`, `is_secret_entry`.
- `storage.rs` (L0.5): `Durability`, `backup_path`, `write_atomic`, `write_atomic_no_backup`, `write_secret`, `write_json`, `read_json`, `append_line`, `append_jsonl`, `FileLock`, `lock`, `try_lock`.
- `settings.rs` (L0.6): `Settings` (`load(global, project, project_trusted)`, `merged`, `get<T>(dotted_key)`, `check`, `diagnostics`), `Diagnostic`, `parse_jsonc`, `set(path, key, value)`.
- `settings_doc.rs`: `KEYS` (every settings key with docs), `CHOICES`, `check(value)`, `documented(key)`, `render()` (source of `docs/config.md`).
- `logging.rs` (L0.7): `init(&Paths, trace) -> std::io::Result<WorkerGuard>`.
- `redact.rs` (L6.6): `Secrets`, `Redactor` (`new(source)`, `with_patterns(on)`, `apply(text) -> Option<String>`).
- `git.rs` (L6.5): `command`, `bookkeeping`, `command_in(dir)`, `bookkeeping_in(dir)`, `pin(root, common_dir)`, `pin_repo(dir)`, `pinned(dir)`.
- `platform.rs` (L11.14): `home`, `bash`, `sh`, `TASK_VARS`, `drop_task_vars`, `own_group`, `set_create_mode`, `set_mode`, `canonicalize`, `slash_path`, `link_dir`, `unlink_dir` (unix and Windows variants).

## Invariants
- `write_atomic` replaces a file so readers see the old or new file, never a torn one, and keeps the old one as `<path>.bak`. Writes follow symlinks: the path is canonicalized, the symlink's target is replaced, and `.bak` sits next to the target (`read_json` looks for it there too). Use `write_atomic_no_backup` for user-owned files where `<path>.bak` may be the user's own.
- `write_secret` creates the file `0600` from its first byte and the parent dir `0700`; always durable.
- `read_json`: missing file → `None`; a corrupt primary with a parsable `.bak` restores the backup.
- `append_line` / `append_jsonl` write one line with a single `write` on an `O_APPEND` fd, so lines never interleave or tear.
- `lock` takes an exclusive cross-process lock on `<path>.lock`, released on drop; `try_lock` returns `None` when held.
- Settings loading never fails: problems become diagnostics. The project file is merged on top only when the project is trusted (project wins), except for the `GLOBAL_ONLY` top-level keys (`permissions`, `sandbox`): a project file's values for them are removed and reported as a diagnostic. A wrong-typed value is reported and treated as unset.
- In debug builds `Settings::get` panics on a key missing from `settings_doc::KEYS`; a test fails when `docs/config.md` is stale.
- `settings::set` edits one key in place, keeps comments and layout, and refuses to touch a file that doesn't parse. It saves with `write_atomic`, so it leaves a `<path>.bak` next to the user's settings file.
- Every git command novacode itself runs is built here: `command` adds `-c core.fsmonitor=false` (callers that read status or diffs add `--ignore-submodules=all` themselves); `bookkeeping*` also adds `-c core.hooksPath=/dev/null -c core.autocrlf=false` and is never for user-requested commands. `command_in` sets `GIT_COMMON_DIR` to the pinned common dir (deepest pinned root wins), so a `.git/commondir` planted later by a sandboxed command is ignored. Pin before any model command runs in the project.
- `Redactor` reads secrets on every use, so refreshed tokens are still masked; matches become `[redacted: <source>]`. Secret values shorter than 8 bytes (`MIN_LEN`) are never masked. Pattern masking (`security.maskSecrets`) is opt-in via `with_patterns`.
- Process kills are not here; they live in `novacode_tools::process`.

## Side Effects
- Disk: atomic writes, `.bak` files, `.lock` files, JSONL appends, chmod (`set_mode`), dir symlinks/junctions (`link_dir`).
- Logging: daily-rotated files in `<home>/logs`; level from `NOVACODE_LOG` (EnvFilter syntax) when set; otherwise `info`, or `debug` when `trace` is on.
- Processes: builds `git`, `bash`, `sh` `Command`s; `own_group` puts children in their own process group; `drop_task_vars` strips `NOVACODE_TASK`, `NOVACODE_TASK_TOKEN`, `NOVACODE_SUPER_DIR`.
- Global state: the git pin table (`static PINS: Mutex<Vec<(root, common_dir)>>`) is process-wide.
- Reads environment: `NOVACODE_HOME`, `HOME`, `NOVACODE_LOG`; on Windows also `USERPROFILE`, `NOVACODE_BASH` and `PATH` (to find Git Bash).

## Verification
- Run: `cargo test -p novacode-base`
