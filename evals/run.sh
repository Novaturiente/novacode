#!/bin/sh
# Manual evals (L11.5): real tasks against a real model. Not part of check.sh (costs money).
# Usage: evals/run.sh MODEL [TASK...]   e.g. evals/run.sh claude/claude-opus-5-5:low
# Each task: files/ (the repo), prompt.md (the ask), check.sh (exit 0 = pass, run in the repo).
# Optional: prompt2.md (a second prompt that continues the session with -c), mcp.json
# (copied into the isolated home, `$EVAL_TASK_DIR` replaced by the task's absolute path).
# EVAL_RUNS=N repeats each task N times (default 1).
set -u
caller=$(pwd)
cd "$(dirname "$0")"
here=$(pwd)
model="${1:?usage: evals/run.sh MODEL [TASK...]}"; shift
bin="${NOVACODE:-novacode}"
case "$bin" in /*) ;; */*) bin="$caller/$bin" ;; esac
real_home="${NOVACODE_HOME:-$HOME/.novacode}"
runs="${EVAL_RUNS:-1}"
[ $# -gt 0 ] || set -- $(for d in */; do [ -f "$d/prompt.md" ] && basename "$d"; done)

# Sum the token usage of every assistant message in the JSONL event files and write
# the last reply's text to $1. Prints "in=N out=N cache=N".
tokens() { # reply-file events-file...
  python3 - "$@" <<'EOF'
import json, sys
tot = {"input": 0, "output": 0, "cache_read": 0}
reply = ""
for path in sys.argv[2:]:
    try:
        lines = open(path).read().splitlines()
    except OSError:
        continue
    for line in lines:
        try:
            m = json.loads(line)
        except ValueError:
            continue
        e = m.get("event") if m.get("type") == "event" else None
        if not e or e.get("type") != "message" or e.get("role") != "assistant":
            continue
        for k in tot:
            tot[k] += e.get("usage", {}).get(k, 0)
        reply = "".join(b.get("text", "") for b in e.get("content", []) if b.get("type") == "text")
open(sys.argv[1], "w").write(reply + "\n")
print("in=%d out=%d cache=%d" % (tot["input"], tot["output"], tot["cache_read"]))
EOF
}

pass=0; n=0
for t in "$@"; do
  tpass=0
  i=1
  while [ "$i" -le "$runs" ]; do
    work=$(mktemp -d "/tmp/nc-eval-$t.XXXX")
    cp -r "$t/files/." "$work/"
    (cd "$work" && git init -q && git add -A && git -c user.email=e@x -c user.name=eval commit -qm start)
    # Isolated home: only the login is shared, so personal skills, hooks, extensions,
    # MCP servers and context files don't change the result. HOME too, for ~/.claude
    # and ~/.agents/skills.
    iso=$(mktemp -d "/tmp/nc-eval-home-$t.XXXX")
    [ -f "$real_home/auth.json" ] && ln -s "$real_home/auth.json" "$iso/auth.json"
    [ -f "$t/mcp.json" ] && sed "s|\$EVAL_TASK_DIR|$here/$t|g" "$t/mcp.json" >"$iso/mcp.json"
    # stdin from /dev/null: `-p` reads piped stdin into the prompt and would wait on an open pipe.
    agent() { (cd "$work" && HOME="$iso" NOVACODE_HOME="$iso" "$bin" -m "$model" --json "$@" </dev/null); }
    start=$(date +%s)
    if [ -f "$t/prompt2.md" ]; then
      agent -p "$(cat "$t/prompt.md")" >"$work/.events.jsonl" 2>"$work/.stderr"
      agent -c -p "$(cat "$t/prompt2.md")" >"$work/.events2.jsonl" 2>"$work/.stderr2"
    else
      agent --no-session -p "$(cat "$t/prompt.md")" >"$work/.events.jsonl" 2>"$work/.stderr"
    fi
    secs=$(( $(date +%s) - start ))
    toks=$(tokens "$work/.reply" "$work/.events.jsonl" "$work/.events2.jsonl")
    if (cd "$work" && sh "$here/$t/check.sh" >/dev/null 2>&1); then r=PASS; tpass=$((tpass+1)); else r=FAIL; fi
    printf '%-20s %s  %4ss  %s  %s\n' "$t" "$r" "$secs" "$toks" "$work"
    rm -rf "$iso"
    i=$((i+1))
  done
  [ "$runs" -gt 1 ] && printf '%-20s %s/%s passed\n' "$t" "$tpass" "$runs"
  pass=$((pass+tpass)); n=$((n+runs))
done
echo "$pass/$n passed with $model"
[ "$pass" = "$n" ]
