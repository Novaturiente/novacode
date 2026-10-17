#!/bin/sh
# The check every build item must pass (L11.7): format, lint, tests.
set -eu
cd "$(dirname "$0")/.."
cargo fmt --all --check
cargo clippy --workspace --all-targets -q -- -D warnings
cargo test --workspace -q
