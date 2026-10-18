#!/bin/sh
# Manual evals (L11.5): real tasks against a real model. Not part of check.sh (costs money).
# Usage: evals/run.sh MODEL [TASK...]   e.g. evals/run.sh claude/claude-opus-5-5:low
# Each task: files/ (the repo), prompt.md (the ask), check.sh (exit 0 = pass, run in the repo).
set -u
cd "$(dirname "$0")"
here=$(pwd)
model="${1:?usage: evals/run.sh MODEL [TASK...]}"; shift
bin="${NOVACODE:-novacode}"
[ $# -gt 0 ] || set -- $(for d in */; do [ -f "$d/prompt.md" ] && basename "$d"; done)
pass=0; n=0
for t in "$@"; do
  work=$(mktemp -d "/tmp/nc-eval-$t.XXXX")
  cp -r "$t/files/." "$work/"
  (cd "$work" && git init -q && git add -A && git -c user.email=e@x -c user.name=eval commit -qm start)
  start=$(date +%s)
  prompt=$(cat "$t/prompt.md")
  (cd "$work" && "$bin" --no-session -m "$model" -p "$prompt" >"$work/.reply" 2>"$work/.stderr")
  secs=$(( $(date +%s) - start ))
  if (cd "$work" && sh "$here/$t/check.sh" >/dev/null 2>&1); then r=PASS; pass=$((pass+1)); else r=FAIL; fi
  n=$((n+1))
  printf '%-10s %s  %3ss  %s\n' "$t" "$r" "$secs" "$work"
done
echo "$pass/$n passed with $model"
[ "$pass" = "$n" ]
