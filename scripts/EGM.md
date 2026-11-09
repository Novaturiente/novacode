# scripts

## Overview
Developer, release and install scripts for novacode. Most `cd` to the repo root first (`check.sh`, `build-musl.sh`, `bench.sh`), so they can be run from anywhere.

## Interface
- `check.sh` (L11.7): the check every build item must pass: `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -q -- -D warnings`, `cargo test --workspace -q`. CI runs it (`.github/workflows/ci.yml`).
- `build-musl.sh [host]` (L11.7): static `x86_64-unknown-linux-musl` release build of `-p novacode`. With a host argument, copies it to `host:~/.local/bin/novacode` over `ssh`/`scp`.
- `bench.sh [path/to/novacode]` (L11.12): performance and resource budgets against a release binary (default `${CARGO_TARGET_DIR:-target}/release/novacode`). Budgets: `START_MS=150`, `HOOK_US=1000`, `FRAME_MS=16`, `IDLE_MB=50`, `CHILD_MB=30`, `EXT_MB=2`, `IDLE_CPU_PCT=1`. Run by `.github/workflows/release.yml` on tags and warning-only in CI.
- `gen-catalog.py [api.json]` (L2.10): regenerates `crates/novacode-ai/data/models.json` from models.dev (or a local `api.json`), keeping only providers in `data/profiles.json`, only tool-calling models, and only the catalog fields.
- `install.sh` (L11.7): `curl … | sh` installer for the latest GitHub release (Linux x86_64/aarch64 musl, macOS arm64). Env: `NOVACODE_REPO`, `NOVACODE_BIN_DIR` (default `~/.local/bin`).
- `install.ps1` (L11.14): `irm … | iex` installer for 64-bit Windows (`x86_64-pc-windows-msvc`). Env: `NOVACODE_REPO`, `NOVACODE_BIN_DIR` (default `%LOCALAPPDATA%\novacode\bin`), `NOVACODE_VERSION`.

## Invariants
- `check.sh` is the project's pass/fail gate; it uses `set -eu` and stops at the first failing step.
- `build-musl.sh` needs the Rust musl target and a musl C compiler (`musl-gcc` on `PATH`, `~/.local/musl/bin/musl-gcc`, or `MUSL_CC`); it fails if the result is not a static binary.
- `bench.sh` exits 1 when a budgeted row is over budget or the hook or frame bench test produced no number, 2 when the binary is missing; `BENCH_WARN_ONLY=1` prints `OVER` rows but exits 0. Report-only rows never fail.
- Both installers verify the release asset's `.sha256` checksum and install nothing on mismatch. They never touch `~/.novacode`.
- `gen-catalog.py` output is compact, key-sorted JSON with a trailing newline.

## Side Effects
- `check.sh`, `bench.sh`: write build output under `target/` (or `CARGO_TARGET_DIR`).
- `build-musl.sh`: writes build output under `target/` (or `CARGO_TARGET_DIR`) and checks `<that dir>/x86_64-unknown-linux-musl/release/novacode`. A `build.target-dir` from a cargo config file is not followed.
- `build-musl.sh host`: remote `ssh`/`scp` writes to `~/.local/bin/novacode` on that host.
- `bench.sh`: temp homes under `mktemp -d` (removed on exit); starts `novacode --rpc` processes, Python fake extension/MCP servers and `cargo test --release -- --ignored` bench tests.
- `gen-catalog.py`: network fetch of `https://models.dev/api.json`; overwrites `crates/novacode-ai/data/models.json`.
- `install.sh` / `install.ps1`: network calls to the GitHub API and release downloads; write the binary to the bin dir. `install.ps1` also adds the bin dir to the user `PATH`.

## Verification
- Run: `scripts/check.sh`
