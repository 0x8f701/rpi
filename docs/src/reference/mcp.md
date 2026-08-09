# Model Context Protocol (MCP) client

`rpi` embeds a Model Context Protocol (MCP) **client**: session-scoped
JSON-RPC 2.0 over a stdio child process (Content-Length framing, mirroring
the LSP client). The `mcp` tool discovers servers and calls their tools;
servers are declared under `settings.mcpServers` in a Grok-compatible
`[mcp_servers.<name>]` shape.

Source: `crates/pi-coding/src/mcp.rs`.

## Configuration

```json
{
  "mcpServers": [
    {
      "name": "my-tools",
      "transport": "stdio",
      "command": "npx",
      "args": ["-y", "my-mcp-server"],
      "env": { "TOKEN": "$MY_TOKEN" },
      "disabled": false
    }
  ]
}
```

| Key | Meaning |
|-----|---------|
| `name` | Server name used by `mcp list_tools <server>` / `mcp call <server>`. |
| `transport` | `stdio` (a child process from `command`/`args`/`env`) or `sse` (an http(s) endpoint at `url`). The client transport in this build is stdio; an `sse` entry parses and round-trips (so Grok/Claude configs survive a settings write) but `call`/`list_tools` against it report the limitation explicitly. |
| `command`/`args` | stdio server command line. |
| `env` | Extra environment for the stdio child. Never echoed into tool output; `$VAR`/`${VAR}` references expand from the process environment. |
| `url` | SSE endpoint (accepted, not connected in this build). |
| `disabled` | `true` (Cursor-compatible) filters the server out at configure time: it never spawns, has no session slot, and never appears in `mcp list_servers`. |

Configured `env` values are never echoed into tool output, and a server's
stderr tail is only surfaced in initialize-failure diagnostics with secret
patterns redacted first.

## The `mcp` tool

```text
mcp list_servers                  # configured servers (live sessions marked)
mcp list_tools <server>           # list a server's tools with descriptions
mcp call <server> <tool> [args]   # invoke a server tool (JSON object or JSON string)
```

- `list_servers` renders name, transport (stdio command line or sse URL),
  and a live marker for servers with a spawned session.
- `list_tools` pages through `tools/list` (up to 512 tools across 32 pages)
  and renders a name+description table.
- `call` validates the tool name client-side against the known list (or via
  the server's `tools/search` extension when advertised) and renders
  `tools/call` results as bounded text (32 KiB cap). `args` must be a JSON
  object or a JSON string that parses to one.

Tool capability: `Write` (servers may mutate external state — the prompt
guidelines tell the model to review call arguments before invoking).

## Lifecycle

- **Session-scoped, spawn on first use**: the registry holds configured
  servers plus one live client per server, spawned lazily on the first tool
  call and killed on drop — the session owns the registry and `Drop` never
  leaks a child process.
- **Fast-start gate**: the first tool call against a server waits up to
  250 ms (holding the session lock) so sibling calls issued in the same turn
  batch into a single spawn instead of N sequential spawns.
- **Reconnects**: transport failures (spawn, framing, io) are retried with
  capped exponential backoff (100 ms → 1 s) up to 3 attempts per call, then
  surface an actionable error naming the server. JSON-RPC protocol errors
  and request timeouts are **not** retried — the wedged session is dropped so
  the next call respawns a fresh server.
- **Progressive tool discovery**: when a server advertises the `search_tool`
  extension (`capabilities.tools.search_tool` or the older experimental
  location), `tools/call` probes with a `tools/search` request and caches the
  definition instead of loading the full list.

Protocol: MCP `2024-11-05` requested on initialize; the negotiated version is
stored per client. Per-request timeout is 30 s; shutdown gets 5 s and exit 2 s
before the child is killed. Protocol framing is shared with the LSP and DAP
clients (`crates/pi-coding/src/tools/framing.rs`).

## Invariants

- A disabled server is never spawned and never listed.
- Reconnect never reuses a wedged session: JSON-RPC errors and timeouts drop
  the client so the next call starts fresh.
- Server output is bounded (32 KiB results, 64 KiB stderr cap) and redacted;
  configured env values never appear in tool output.
- Spawn/framing failures are retried only when marked transport-level; a
  JSON-RPC refusal (e.g. a server that rejects initialize) fails immediately.

## Related documentation

- [`settings-trust.md`](settings-trust.md) — `mcpServers` settings and env
  expansion
- [`environment-variables.md`](environment-variables.md) — `$VAR` expansion
- [`extensions.md`](extensions.md) — the in-process extension protocol (the
  alternative to MCP for adding tools)
