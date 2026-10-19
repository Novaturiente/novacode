#!/usr/bin/env bash
# L11.12 performance and resource budgets. Run by hand after touching the loop,
# extensions or the TUI; CI runs it on release tags. Exit 1 when over budget.
#   scripts/bench.sh [path/to/novacode]   (default: target/release/novacode)
set -euo pipefail
cd "$(dirname "$0")/.."
BIN=$(realpath "${1:-${CARGO_TARGET_DIR:-target}/release/novacode}")
[ -x "$BIN" ] || { echo "no binary at $BIN (cargo build --release)"; exit 2; }

# Budgets (DECISIONS 2026-10-19).
START_MS=150 HOOK_US=1000 FRAME_MS=16 IDLE_MB=50 CHILD_MB=30 EXT_MB=2 IDLE_CPU_PCT=1

fail=0
row() { # name value budget unit
    local ok=ok
    awk -v v="$2" -v b="$3" 'BEGIN{exit !(v+0 > b+0)}' && { ok=OVER; fail=1; }
    printf "  %-28s %10s %-3s budget %-6s %s\n" "$1" "$2" "$4" "$3" "$ok"
}

TMP=$(mktemp -d)
trap 'kill $(jobs -p) 2>/dev/null || true; rm -rf "$TMP"' EXIT
mkhome() { # dir extensions
    mkdir -p "$1/extensions" "$1/proj"
    echo '{"model": "anthropic/claude-sonnet-4-5"}' >"$1/settings.json"
    for i in $(seq 1 "$2"); do
        cat >"$1/extensions/ext$i.ts" <<EOF
export default (nc: any) => {
  nc.registerTool({ name: "t$i", description: "bench $i", parameters: { type: "object", properties: {} }, execute: async () => "ok" });
  nc.on("before_tool", (e: any) => (e.tool === "nope$i" ? { block: "no" } : undefined));
  nc.ui.setStatus("e$i", "ready");
};
EOF
    done
}
run() { # home args...
    local h=$1; shift
    (cd "$h/proj" && NOVACODE_HOME="$h" ANTHROPIC_API_KEY=bench exec "$BIN" "$@")
}
rss_mb() { awk -v k="$2:" '$1==k{printf "%.1f", $2/1024}' "/proc/$1/status"; }
cpu_ticks() { awk '{print $14+$15}' "/proc/$1/stat"; }

# Idle headless session (`--rpc` is also what a subagent child runs).
idle() { # home -> "rss_mb anon_mb cpu_pct"
    mkfifo "$1/in"
    # Not through run(): a backgrounded function is one more fork, and $! would be the shell.
    (cd "$1/proj" && NOVACODE_HOME="$1" ANTHROPIC_API_KEY=bench exec "$BIN" --rpc --no-session) <"$1/in" >/dev/null 2>&1 &
    local pid=$!
    exec 3>"$1/in"
    sleep 2
    local t0; t0=$(cpu_ticks $pid)
    sleep 3
    local t1 r a; t1=$(cpu_ticks $pid); r=$(rss_mb $pid VmRSS); a=$(rss_mb $pid RssAnon)
    exec 3>&-
    wait $pid 2>/dev/null || true
    echo "$r $a $(awk -v d=$((t1 - t0)) -v hz="$(getconf CLK_TCK)" 'BEGIN{printf "%.1f", d*100/hz/3}')"
}

echo "novacode bench: $BIN"
mkhome "$TMP/h0" 0
mkhome "$TMP/h20" 20
run "$TMP/h20" --profile-startup >/dev/null 2>&1 # warm the bytecode cache
best=999999
for _ in 1 2 3 4 5; do
    s=$(date +%s%N)
    run "$TMP/h20" --profile-startup >/dev/null 2>&1
    ms=$((($(date +%s%N) - s) / 1000000))
    ((ms < best)) && best=$ms
done
row "start, 20 extensions" "$best" $START_MS ms

read -r rss0 anon0 cpu0 < <(idle "$TMP/h0")
read -r _ anon20 _ < <(idle "$TMP/h20")
row "idle session RSS" "$rss0" $IDLE_MB MB
# A child shares the binary's code pages with its parent (page cache); its own cost is anonymous memory.
row "subagent child (anon RSS)" "$anon0" $CHILD_MB MB
row "per extension (anon RSS)" "$(awk -v a="$anon20" -v b="$anon0" 'BEGIN{printf "%.2f", (a-b)/20}')" $EXT_MB MB
row "idle CPU" "$cpu0" $IDLE_CPU_PCT %

bench_test() { # crate test -> value
    cargo test -q --release -p "$1" -- --ignored --exact "$2" --nocapture 2>&1 | awk -v k="$3" '$1=="BENCH" && $2==k{print $3}'
}
hook=$(bench_test novacode-script ext::tests::bench_hook hook_us)
row "hook round trip" "${hook:-?}" $HOOK_US µs
frame=$(bench_test novacode-tui bench_frame frame_p99_ms)
row "keypress to frame p99" "${frame:-?}" $FRAME_MS ms
[ -n "$hook" ] && [ -n "$frame" ] || fail=1

exit $fail
