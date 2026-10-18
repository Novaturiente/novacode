#!/usr/bin/env python3
"""Regenerate crates/novacode-ai/data/models.json (L2.10) from models.dev.

Keeps only the providers novacode has a profile for (data/profiles.json) and
only the fields the catalog reads. Run: scripts/gen-catalog.py [api.json]
"""
import json, sys, urllib.request, pathlib

root = pathlib.Path(__file__).resolve().parent.parent / "crates/novacode-ai/data"
src = sys.argv[1] if len(sys.argv) > 1 else None
raw = json.load(open(src)) if src else json.load(urllib.request.urlopen("https://models.dev/api.json"))
profiles = json.load(open(root / "profiles.json"))
wanted = sorted({p.get("catalog", pid) for pid, p in profiles.items()})
KEEP = ["name", "family", "attachment", "reasoning", "reasoning_options", "tool_call", "modalities", "limit", "cost", "release_date"]
out = {}
for pid in wanted:
    if pid not in raw:
        continue
    models = {}
    for mid, m in sorted(raw[pid]["models"].items()):
        if not m.get("tool_call"):
            continue
        k = {f: m[f] for f in KEEP if f in m}
        if "modalities" in k:
            k["modalities"] = {"input": k["modalities"].get("input", [])}
        if "cost" in k:
            k["cost"] = {c: k["cost"][c] for c in ("input", "output", "cache_read", "cache_write") if c in k["cost"]}
        models[mid] = k
    out[pid] = {"models": models}
(root / "models.json").write_text(json.dumps(out, separators=(",", ":"), sort_keys=True) + "\n")
print(f"{len(out)} providers, {sum(len(p['models']) for p in out.values())} models")
