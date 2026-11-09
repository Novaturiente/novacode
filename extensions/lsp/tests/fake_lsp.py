#!/usr/bin/env python3
"""Scripted language server for lsp.rs tests. Usage: fake_lsp.py MODE

loading    answers initialize, then stays busy (a $/progress that never ends)
crash      exits before answering initialize
unhealthy  reports rust-analyzer-style health "warning"; publishes no errors
slow       answers initialize after 3 s
errors     publishes one error for every opened or changed file
"""
import json
import sys
import time

MODE = sys.argv[1]
if MODE == "crash":
    sys.exit(1)

out, inp = sys.stdout.buffer, sys.stdin.buffer


def send(m):
    b = json.dumps(m).encode()
    out.write(b"Content-Length: %d\r\n\r\n" % len(b) + b)
    out.flush()


def recv():
    n = 0
    while True:
        line = inp.readline()
        if not line:
            sys.exit(0)
        line = line.strip()
        if not line:
            return json.loads(inp.read(n))
        if line.lower().startswith(b"content-length:"):
            n = int(line.split(b":")[1])


while True:
    m = recv()
    method = m.get("method")
    if method == "initialize":
        if MODE == "slow":
            time.sleep(3)
        send({"jsonrpc": "2.0", "id": m["id"], "result": {"capabilities": {}}})
        if MODE == "loading":
            send({"jsonrpc": "2.0", "method": "$/progress", "params": {"token": "idx", "value": {"kind": "begin"}}})
        if MODE == "unhealthy":
            send({"jsonrpc": "2.0", "method": "experimental/serverStatus",
                  "params": {"quiescent": True, "health": "warning", "message": "no std sources"}})
    elif method in ("textDocument/didOpen", "textDocument/didChange"):
        diags = []
        if MODE == "errors":
            diags = [{"range": {"start": {"line": 0, "character": 0}}, "severity": 1, "message": "boom"}]
        uri = m["params"]["textDocument"]["uri"]
        send({"jsonrpc": "2.0", "method": "textDocument/publishDiagnostics", "params": {"uri": uri, "diagnostics": diags}})
