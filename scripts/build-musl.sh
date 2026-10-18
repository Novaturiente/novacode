#!/bin/sh
# Static Linux x86_64 binary (L11.7): one file to copy to another machine, no glibc needed.
# Needs the Rust musl std (rustup target add x86_64-unknown-linux-musl) and a musl C
# compiler for the C deps (SQLite, QuickJS, ring): musl-gcc on PATH, or MUSL_CC.
# Usage: scripts/build-musl.sh [host]   (with a host: scp to host:~/.local/bin/novacode)
set -eu
cd "$(dirname "$0")/.."
cc="${MUSL_CC:-$(command -v musl-gcc || echo "$HOME/.local/musl/bin/musl-gcc")}"
[ -x "$cc" ] || { echo "no musl C compiler: install musl (musl-gcc) or set MUSL_CC" >&2; exit 1; }
target=x86_64-unknown-linux-musl
CC_x86_64_unknown_linux_musl="$cc" \
  CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_RUSTFLAGS="-C target-feature=+crt-static" \
  cargo build --release --locked --target "$target" -p novacode
bin="target/$target/release/novacode"
file "$bin" | grep -q "static" || { echo "not a static binary: $(file "$bin")" >&2; exit 1; }
ls -lh "$bin"
if [ $# -gt 0 ]; then
  ssh "$1" 'mkdir -p ~/.local/bin'
  scp -q "$bin" "$1:.local/bin/novacode.new"
  ssh "$1" 'mv ~/.local/bin/novacode.new ~/.local/bin/novacode && ~/.local/bin/novacode --version'
fi
