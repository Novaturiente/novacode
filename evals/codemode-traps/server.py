"""A stdio MCP server for the codemode-traps eval (L11.5, L7.17).

Its tools test how codemode presents itself: a required argument with an unusual
name and an enum (find_orders, plain text result), a declared outputSchema with
structuredContent (order_totals) and an image result (receipt_image).
"""
import base64, json, struct, sys, zlib

ORDERS = {  # customer_ref -> {order id: (status, total in cents, receipt RGB)}
    "C-4471": {
        "ORD-2201": ("open", 3100, (255, 0, 0)),
        "ORD-2202": ("shipped", 1250, (255, 0, 0)),
        "ORD-2203": ("cancelled", 9900, (0, 160, 0)),
        "ORD-2205": ("shipped", 4999, (0, 0, 255)),
        "ORD-2209": ("shipped", 870, (255, 220, 0)),
    },
    "C-1180": {"ORD-1104": ("shipped", 20000, (0, 160, 0))},
}
BY_ID = {oid: o for orders in ORDERS.values() for oid, o in orders.items()}
STATUSES = ["open", "shipped", "cancelled"]

TOOLS = [
    {
        "name": "find_orders",
        "description": "List a customer's order ids with the given status, one per line.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "customer_ref": {"type": "string", "description": "Customer reference, like C-0000"},
                "status": {"type": "string", "enum": STATUSES},
            },
            "required": ["customer_ref", "status"],
        },
    },
    {
        "name": "order_totals",
        "description": "Totals of the given orders, in cents.",
        "inputSchema": {
            "type": "object",
            "properties": {"order_ids": {"type": "array", "items": {"type": "string"}}},
            "required": ["order_ids"],
        },
        "outputSchema": {
            "type": "object",
            "properties": {
                "currency": {"type": "string"},
                "orders": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {"id": {"type": "string"}, "total_cents": {"type": "integer"}},
                        "required": ["id", "total_cents"],
                    },
                },
            },
            "required": ["currency", "orders"],
        },
    },
    {
        "name": "receipt_image",
        "description": "The scanned receipt of one order, as an image.",
        "inputSchema": {
            "type": "object",
            "properties": {"order_id": {"type": "string"}},
            "required": ["order_id"],
        },
    },
]
SCHEMAS = {t["name"]: t["inputSchema"] for t in TOOLS}


def png(rgb, size=48):
    """A solid-colour RGB PNG, built by hand (no PIL)."""
    def chunk(kind, data):
        body = kind + data
        return struct.pack(">I", len(data)) + body + struct.pack(">I", zlib.crc32(body) & 0xFFFFFFFF)
    row = b"\x00" + bytes(rgb) * size
    ihdr = struct.pack(">IIBBBBB", size, size, 8, 2, 0, 0, 0)
    return b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", ihdr) + chunk(b"IDAT", zlib.compress(row * size)) + chunk(b"IEND", b"")


def error(text):
    return {"content": [{"type": "text", "text": text}], "isError": True}


def check_args(name, args):
    """A clear error for a wrong, missing or out-of-enum argument, else None."""
    schema = SCHEMAS[name]
    props = schema["properties"]
    usage = ", ".join(
        "%s (%s)" % (k, "one of " + "|".join(p["enum"]) if "enum" in p else p["type"]) for k, p in props.items()
    )
    for k in args:
        if k not in props:
            return error("%s: unknown argument '%s'. Arguments: %s" % (name, k, usage))
    for k in schema["required"]:
        if k not in args:
            return error("%s: missing required argument '%s'. Arguments: %s" % (name, k, usage))
    for k, p in props.items():
        if k in args and "enum" in p and args[k] not in p["enum"]:
            return error("%s: '%s' must be one of %s, got %r" % (name, k, ", ".join(p["enum"]), args[k]))
    return None


def call(name, args):
    bad = check_args(name, args)
    if bad:
        return bad
    if name == "find_orders":
        orders = ORDERS.get(args["customer_ref"])
        if orders is None:
            return error("find_orders: no customer with reference %r" % args["customer_ref"])
        ids = [oid for oid, o in orders.items() if o[0] == args["status"]]
        return {"content": [{"type": "text", "text": "\n".join(ids) if ids else "(no orders)"}]}
    if name == "order_totals":
        ids = args["order_ids"]
        if not isinstance(ids, list) or not all(isinstance(i, str) for i in ids):
            return error("order_totals: 'order_ids' must be an array of strings")
        unknown = [i for i in ids if i not in BY_ID]
        if unknown:
            return error("order_totals: unknown order ids: %s" % ", ".join(unknown))
        data = {"currency": "EUR", "orders": [{"id": i, "total_cents": BY_ID[i][1]} for i in ids]}
        return {"content": [{"type": "text", "text": json.dumps(data)}], "structuredContent": data}
    if name == "receipt_image":
        o = BY_ID.get(args["order_id"])
        if o is None:
            return error("receipt_image: unknown order id %r" % args["order_id"])
        data = base64.b64encode(png(o[2])).decode()
        return {"content": [{"type": "image", "data": data, "mimeType": "image/png"}]}
    return None


for line in sys.stdin:
    m = json.loads(line)
    if "id" not in m:
        continue
    r = None
    if m["method"] == "initialize":
        r = {"protocolVersion": "2025-06-18", "capabilities": {"tools": {}}, "serverInfo": {"name": "shop", "version": "1"}}
    elif m["method"] == "tools/list":
        r = {"tools": TOOLS}
    elif m["method"] == "tools/call" and m["params"]["name"] in SCHEMAS:
        r = call(m["params"]["name"], m["params"].get("arguments") or {})
    if r is None:
        print(json.dumps({"jsonrpc": "2.0", "id": m["id"], "error": {"code": -32601, "message": "unknown method"}}), flush=True)
        continue
    print(json.dumps({"jsonrpc": "2.0", "id": m["id"], "result": r}), flush=True)
