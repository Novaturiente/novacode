"""A stdio MCP server with one tool, build_id, for the mcp-lookup eval."""
import json, sys

for line in sys.stdin:
    m = json.loads(line)
    if "id" not in m:
        continue
    if m["method"] == "initialize":
        r = {"protocolVersion": "2025-06-18", "capabilities": {"tools": {}}, "serverInfo": {"name": "vault", "version": "1"}}
    elif m["method"] == "tools/list":
        r = {"tools": [{"name": "build_id", "description": "The current build id of this project", "inputSchema": {"type": "object", "properties": {}}}]}
    elif m["method"] == "tools/call" and m["params"]["name"] == "build_id":
        r = {"content": [{"type": "text", "text": "nc-7f3a91c2"}]}
    else:
        print(json.dumps({"jsonrpc": "2.0", "id": m["id"], "error": {"code": -32601, "message": "unknown method"}}), flush=True)
        continue
    print(json.dumps({"jsonrpc": "2.0", "id": m["id"], "result": r}), flush=True)
