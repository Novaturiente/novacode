#!/bin/sh
# The check every build item must pass (L11.7): format, lint, tests.
set -eu
cd "$(dirname "$0")/.."
cargo fmt --all --check
cargo clippy --workspace --all-targets -q -- -D warnings
cargo test --workspace -q
# Super mode is off by default (L10.9); keep it building and passing.
cargo clippy -p novacode --all-targets --features super -q -- -D warnings
cargo test -p novacode --features super -q
