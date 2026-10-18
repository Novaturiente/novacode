# Fake OAuth-protected MCP server for tests: RFC 9728 + 8414 metadata, DCR, PKCE, refresh.
import base64, hashlib, json, sys, urllib.parse
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

S = {"clients": {}, "codes": {}, "access": set(), "refresh": {}, "n": 0}

class H(BaseHTTPRequestHandler):
    def log_message(self, *a): pass
    def base(self): return f"http://127.0.0.1:{self.server.server_port}"
    def send(self, code, body=None, headers=()):
        self.send_response(code)
        for k, v in headers: self.send_header(k, v)
        data = json.dumps(body).encode() if body is not None else b""
        self.send_header("content-type", "application/json"); self.send_header("content-length", str(len(data)))
        self.end_headers(); self.wfile.write(data)
    def body(self):
        return self.rfile.read(int(self.headers.get("content-length", 0))).decode()
    def do_GET(self):
        u = urllib.parse.urlparse(self.path); q = dict(urllib.parse.parse_qsl(u.query))
        if u.path == "/.well-known/oauth-protected-resource/mcp":
            return self.send(200, {"resource": self.base() + "/mcp", "authorization_servers": [self.base() + "/auth"], "scopes_supported": ["tools"]})
        if u.path == "/.well-known/oauth-authorization-server/auth":
            b = self.base() + "/auth"
            return self.send(200, {"issuer": b, "authorization_endpoint": b + "/authorize", "token_endpoint": b + "/token",
                                   "registration_endpoint": b + "/register", "response_types_supported": ["code"], "code_challenge_methods_supported": ["S256"]})
        if u.path == "/auth/authorize":
            assert q["client_id"] in S["clients"] and q["code_challenge_method"] == "S256" and q["resource"].endswith("/mcp"), q
            code = "code%d" % len(S["codes"]); S["codes"][code] = q
            loc = q["redirect_uri"] + "?" + urllib.parse.urlencode({"code": code, "state": q["state"]})
            return self.send(302, None, [("location", loc)])
        self.send(404, {})
    def tokens(self):
        S["n"] += 1; a, r = "at%d" % S["n"], "rt%d" % S["n"]
        S["access"] = {a}; S["refresh"][r] = True
        return {"access_token": a, "refresh_token": r, "token_type": "Bearer", "expires_in": 3600}
    def do_POST(self):
        u = urllib.parse.urlparse(self.path); raw = self.body()
        if u.path == "/auth/register":
            m = json.loads(raw); cid = "client%d" % len(S["clients"]); S["clients"][cid] = m
            return self.send(201, {"client_id": cid, **m})
        if u.path == "/auth/token":
            f = dict(urllib.parse.parse_qsl(raw))
            if f["grant_type"] == "authorization_code":
                q = S["codes"].pop(f["code"])
                ch = base64.urlsafe_b64encode(hashlib.sha256(f["code_verifier"].encode()).digest()).rstrip(b"=").decode()
                assert ch == q["code_challenge"] and f["redirect_uri"] == q["redirect_uri"] and f["client_id"] == q["client_id"]
                return self.send(200, self.tokens())
            if f["grant_type"] == "refresh_token" and S["refresh"].pop(f["refresh_token"], None):
                return self.send(200, self.tokens())
            return self.send(400, {"error": "invalid_grant"})
        if u.path == "/revoke":  # test hook: invalidate the current access token
            S["access"] = set(); return self.send(200, {})
        if u.path == "/mcp":
            tok = self.headers.get("authorization", "").removeprefix("Bearer ")
            if tok not in S["access"]:
                return self.send(401, {}, [("www-authenticate", f'Bearer resource_metadata="{self.base()}/.well-known/oauth-protected-resource/mcp"')])
            m = json.loads(raw)
            if "id" not in m: return self.send(202)
            meth = m["method"]
            if meth == "initialize": r = {"protocolVersion": "2025-06-18", "capabilities": {"tools": {}}, "serverInfo": {"name": "fake"}}
            elif meth == "tools/list": r = {"tools": [{"name": "whoami", "description": "Who", "inputSchema": {"type": "object"}}]}
            elif meth == "tools/call": r = {"content": [{"type": "text", "text": "token " + tok}]}
            else: r = {}
            return self.send(200, {"jsonrpc": "2.0", "id": m["id"], "result": r})
        self.send(404, {})

srv = ThreadingHTTPServer(("127.0.0.1", 0), H)
print(srv.server_port, flush=True)
srv.serve_forever()
