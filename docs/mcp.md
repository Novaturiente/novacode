# MCP servers

This page shows how to connect MCP servers (local programs or remote HTTP servers) to novacode, how the model uses their tools, and how to log in to servers that use OAuth.

## Quick start

```sh
# a local (stdio) server
novacode mcp add files -- npx -y @modelcontextprotocol/server-filesystem /home/me/docs

# a remote (HTTP) server
novacode mcp add notes --url https://mcp.example.com/mcp

# see what's configured and whether it connects
novacode mcp list
```

Inside a session, `/mcp` shows each server's status, tool count, description and the path of its log (`~/.novacode/logs/mcp-<name>.log`).

## Config files

The format is the same `mcpServers` object Claude Code uses.

| File | Notes |
|---|---|
| `mcp.json` in an installed [package](packages.md) | Lowest priority. |
| `~/.novacode/mcp.json` | Yours. Written by `novacode mcp add` and `remove`. |
| `.mcp.json` in your project | Read only in [trusted](security.md) folders. novacode looks in the current folder and then upward to the nearest one. Highest priority. |

If the same server name appears more than once, the later file in this order wins: package, then yours, then the project's. A project `.novacode/mcp.json` is **not** read; use `.mcp.json`. In an untrusted folder `novacode mcp list` tells you the project file is ignored.

Server names can't be empty or contain `__` (a warning is logged and the entry is skipped). Set `"disabled": true` on a server to switch it off without deleting it.

```json
{
  "mcpServers": {
    "files": {
      "command": "npx",
      "args": ["-y", "@modelcontextprotocol/server-filesystem", "/home/me/docs"],
      "description": "Read and search my notes folder"
    },
    "notes": {
      "url": "https://mcp.example.com/mcp",
      "headers": { "X-Team": "${TEAM_ID:-default}" },
      "exposure": "listed"
    }
  }
}
```

### Server fields

A server is stdio if it has `command`, and HTTP if it has `url`. If both are set, `url` wins.

| Field | Applies to | Meaning |
|---|---|---|
| `command` | stdio | Program to run. |
| `args` | stdio | Arguments. |
| `env` | stdio | Extra environment variables. |
| `cwd` | stdio | Working folder, relative to your session's folder. Default: that folder. |
| `url` | HTTP | The server's streamable-HTTP endpoint. |
| `headers` | HTTP | Extra headers on every request. |
| `description` | both | One line shown to the model next to the server's name, so it knows when to use it. |
| `exposure` | both | How the server's tools reach the model. Default `codemode-only`. See below. |
| `tools` | both | Per-tool overrides: `{ "search": { "exposure": "listed" } }`. |
| `timeout` | both | Seconds without an answer or progress before a call fails. Default `120`. |
| `disabled` | both | `true` skips the server. |
| `oauth` | HTTP | OAuth client settings (see below). |

`${VAR}` and `${VAR:-default}` are replaced from your environment in `command`, `args`, `env`, `cwd`, `url` and `headers`.

A server's connect attempt gives up after 60 seconds.

## `novacode mcp` commands

| Command | What it does |
|---|---|
| `novacode mcp list` | Every server with its status, tool count, config file, and its first 15 tools with their exposure. Exits 1 if any server failed. |
| `novacode mcp add <name> -- <command> [args…]` | Add a stdio server to `~/.novacode/mcp.json`. |
| `novacode mcp add <name> --url <url>` | Add an HTTP server. |
| `novacode mcp add … --env KEY=VALUE` | Set an environment variable for the server. Repeat the flag for more. |
| `novacode mcp remove <name>` | Remove a server from `~/.novacode/mcp.json`. |
| `novacode mcp login <name>` | Log in to an HTTP server that uses OAuth. |
| `novacode mcp logout <name>` | Forget that server's saved login. |

`add` and `remove` only edit your own file. To change a server in a project `.mcp.json` or a package, edit that file.

## How the model uses MCP tools

Each tool is named `<server>__<tool>`. How it reaches the model depends on `exposure`:

| Value | Meaning |
|---|---|
| `codemode-only` (default) | Not in the request. The model calls it from a `codemode` script. |
| `listed` | Declared to the model like a built-in tool. |
| `deferred` | Found with `tool_search` when needed. |
| `hidden` | The model can't use it. |

`codemode` is also accepted as a spelling of `codemode-only`. You can set one tool's exposure in the server's `tools` map, or with the setting `tools.<server>__<tool>.exposure`.

**Why `codemode-only` is the default.** A server can have dozens of tools, and declaring all of them costs tokens on every request. With `codemode-only` the model sees one `codemode` tool, plus a line per server (its name and your `description`). It looks tools up on demand, and chains several calls in one script so only the final result comes back to it.

In a script, the model calls an MCP tool like this:

```js
const hits = await mcp.notes.search({ query: "release checklist" });
const page = await mcp.notes["get-page"]({ id: hits[0].id });   // dashed names use brackets
return page;
```

Inside scripts, `searchTools(query)`, `describeTool(name)` and `describeNamespace(server)` show what exists and the argument types. Tool calls from a script go through the same permission rules and hooks as direct calls.

Write a good `description` for each server. The model only knows a `codemode-only` server by its name and that line.

## Connecting and the tool cache

Servers connect in the background, so a session starts at once. novacode caches each server's tool list in `~/.novacode/cache/mcp/<name>.json` after a successful connect, and the next session starts with the same tools whether or not the server is up yet.

The tool list is fixed when the session starts. So the first time you add a server, its tools are not visible in that session until the list has been fetched; start a new session after `novacode mcp list` (or after the first session has connected). A call to a server that isn't connected yet waits for the connection.

## OAuth for remote servers

For an HTTP server that needs a login:

```sh
novacode mcp login notes
```

novacode prints a URL (and tries to open your browser), you approve, and it catches the redirect on a loopback address `http://127.0.0.1:<port>/callback`. On a remote machine, paste the URL the browser ends on. Tokens are saved in `~/.novacode/mcp-auth.json` (mode 0600), keyed by server name and URL, and refreshed when they run out. New sessions use the login.

If a call later fails with a login error, the message tells you to run `novacode mcp login <name>` again. `novacode mcp logout <name>` removes the saved login. If you set an `Authorization` header yourself, that header is used instead.

novacode registers itself with the server automatically when the server allows it. If not, register a client with the server's provider and add it to the config:

```json
{
  "mcpServers": {
    "notes": {
      "url": "https://mcp.example.com/mcp",
      "oauth": {
        "clientId": "abc123",
        "clientSecret": "…",
        "scope": "read write",
        "callbackPort": 8765
      }
    }
  }
}
```

| `oauth` field | Meaning |
|---|---|
| `clientId` | Your registered client id. |
| `clientSecret` | Its secret, if it has one. |
| `scope` | Scopes to ask for. |
| `callbackPort` | Fixed loopback port, for clients registered with an exact redirect URI. Default: any free port. |

Login endpoints must use `https` (loopback addresses are the exception). Only HTTP servers use OAuth; `login` on a stdio server is an error.

## Subagents and MCP

A [subagent](subagents.md) gets no MCP servers unless you grant them. The model can grant servers when it spawns one (`mcp: ["notes"]`), or you can set defaults per agent type:

```json
{ "agents": { "worker": { "mcp": ["notes"] } } }
```

A subagent with MCP servers also gets `codemode`, because that's how it calls them. If `agents.parentPicksTools` is off, the model can't pick servers; only your settings apply. See [Settings](config.md).

## See also

- [Tools](tools.md)
- [Subagents, plan mode and goals](subagents.md)
- [Packages](packages.md)
- [Security and trust](security.md)
- [Settings](config.md)
- [Troubleshooting](troubleshooting.md)
- [Migrating](migrating.md)
