#!/usr/bin/env bash
# L11.12 performance and resource budgets. Run by hand after touching the loop,
# extensions or the TUI; CI runs it on release tags and (warning only) on pull requests.
# Exit 1 when over budget; BENCH_WARN_ONLY=1 prints the OVER rows but exits 0.
# Rows without a budget are report-only and never fail.
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
info() { # name value unit (report only)
    printf "  %-28s %10s %-3s report only\n" "$1" "$2" "$3"
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
starts=()
for _ in $(seq 1 9); do
    s=$(date +%s%N)
    run "$TMP/h20" --profile-startup >/dev/null 2>&1
    starts+=($((($(date +%s%N) - s) / 1000000)))
done
mapfile -t starts < <(printf '%s\n' "${starts[@]}" | sort -n)
# Of 9 sorted runs: the 5th is the median, the 9th the p95 (nearest rank).
row "start, 20 extensions (p50)" "${starts[4]}" $START_MS ms
info "start, 20 extensions (p95)" "${starts[8]}" ms

read -r rss0 anon0 cpu0 < <(idle "$TMP/h0")
read -r _ anon20 _ < <(idle "$TMP/h20")
row "idle session RSS" "$rss0" $IDLE_MB MB
# A child shares the binary's code pages with its parent (page cache); its own cost is anonymous memory.
row "subagent child (anon RSS)" "$anon0" $CHILD_MB MB
row "per extension (anon RSS)" "$(awk -v a="$anon20" -v b="$anon0" 'BEGIN{printf "%.2f", (a-b)/20}')" $EXT_MB MB
row "idle CPU" "$cpu0" $IDLE_CPU_PCT %

# Tree PSS: a parent plus 3 subagent-like children, each with one process extension
# and one stdio MCP server. PSS splits shared pages between the processes that map them,
# so the sum is what the whole tree really costs.
mktree() { # dir
    mkdir -p "$1/proj"
    cat >"$1/ext.py" <<'EOF'
import json, sys
for line in sys.stdin:
    m = json.loads(line)
    if "id" in m:
        r = {} if m.get("method") == "initialize" else None
        print(json.dumps({"jsonrpc": "2.0", "id": m["id"], "result": r}), flush=True)
EOF
    cat >"$1/mcp.py" <<'EOF'
import json, sys
for line in sys.stdin:
    m = json.loads(line)
    if "id" not in m: continue
    if m["method"] == "initialize": r = {"protocolVersion": "2025-06-18", "capabilities": {"tools": {}}, "serverInfo": {"name": "b", "version": "1"}}
    elif m["method"] == "tools/list": r = {"tools": [{"name": "ping", "description": "Ping", "inputSchema": {"type": "object", "properties": {}}}]}
    else: r = {"content": [{"type": "text", "text": "pong"}]}
    print(json.dumps({"jsonrpc": "2.0", "id": m["id"], "result": r}), flush=True)
EOF
    printf '{"model": "anthropic/claude-sonnet-4-5", "processExtensions": {"bench": {"command": "python3", "args": ["%s/ext.py"]}}}\n' "$1" >"$1/settings.json"
    printf '{"mcpServers": {"bench": {"command": "python3", "args": ["%s/mcp.py"]}}}\n' "$1" >"$1/mcp.json"
}
descendants() { # pid -> pid and every descendant, one per line
    echo "$1"
    local c
    for c in $(cat /proc/"$1"/task/*/children 2>/dev/null); do descendants "$c"; done
}
pss_kb() { awk '$1=="Pss:"{print $2}' "/proc/$1/smaps_rollup" 2>/dev/null || echo 0; }
mktree "$TMP/tree"
pids=() fds=()
for i in 0 1 2 3; do
    mkfifo "$TMP/tree/in$i"
    (cd "$TMP/tree/proj" && NOVACODE_HOME="$TMP/tree" ANTHROPIC_API_KEY=bench exec "$BIN" --rpc --no-session) <"$TMP/tree/in$i" >/dev/null 2>&1 &
    pids+=($!)
    exec {fd}>"$TMP/tree/in$i"
    fds+=("$fd")
done
sleep 3
total=0
for p in $(for p in "${pids[@]}"; do descendants "$p"; done | sort -u); do
    total=$((total + $(pss_kb "$p")))
done
info "tree PSS (1+3, ext + MCP)" "$(awk -v k=$total 'BEGIN{printf "%.1f", k/1024}')" MB
for fd in "${fds[@]}"; do exec {fd}>&-; done
for p in "${pids[@]}"; do wait "$p" 2>/dev/null || true; done

bench_test() { # crate test -> value
    cargo test -q --release -p "$1" -- --ignored --exact "$2" --nocapture 2>&1 | awk -v k="$3" '$1=="BENCH" && $2==k{print $3}'
}
hook=$(bench_test novacode-script ext::tests::bench_hook hook_us)
row "hook round trip" "${hook:-?}" $HOOK_US µs
frame=$(bench_test novacode-tui bench_frame frame_p99_ms)
row "keypress to frame p99" "${frame:-?}" $FRAME_MS ms
frame_long=$(bench_test novacode-tui bench_frame_long frame_long_p99_ms)
info "frame p99, 2000 turns open" "${frame_long:-?}" ms
[ -n "$hook" ] && [ -n "$frame" ] || fail=1

[ "${BENCH_WARN_ONLY:-0}" = 1 ] && { [ $fail = 0 ] || echo "over budget (BENCH_WARN_ONLY=1: not failing)"; exit 0; }
exit $fail
