# Model Context Protocol (MCP) client

The Model Context Protocol client connects a session to configured tool servers. The shipped execution transport is **stdio with Content-Length-framed JSON-RPC 2.0**. An `sse` configuration can parse and round-trip, but listing or calling its tools reports the unsupported transport instead of pretending to connect.

## Configuration and commands

Servers are configured under `mcpServers` with a name and transport. A stdio server supplies command, arguments, and environment; an `sse` entry supplies a URL. Disabled entries never spawn and are excluded from the runtime server list.

```text
rpi mcp list
rpi mcp import --source claude --file CONFIG_PATH
rpi mcp import --source cursor --file CONFIG_PATH --local
```

Import supports `claude`, `cursor`, and `auto`. Existing server names are not overwritten unless `--force` is supplied. `--local` chooses project settings; imported configuration still needs review and project admission.

The model-facing `mcp` tool provides `list_servers`, `list_tools` for a server, and `call` for a server/tool with arguments.

## Lifecycle

Clients are session-scoped and start lazily on first use. Same-server calls arriving within the 250-millisecond startup window share startup. Session replacement shuts down initialized clients; configuration remains, and the next call starts a fresh server.

Transport failures have at most three attempts with bounded backoff. Protocol errors and request timeouts are not retried as transport failures. The affected client is dropped so a subsequent call can create a fresh connection.

## Bounds and discovery

Requests time out after **30 seconds**. Tool listing collects at most **512 tools** over **32 pages**. Rendered result output is bounded to **32 KiB**. Initialization requests protocol version `2024-11-05` and records the negotiated version.

Servers advertising `search_tool` support can be queried with `tools/search` for progressive discovery; if the server rejects that extension, the client obtains its full tool list.

## Security

Server processes pass governed spawn and credential-audience checks. Configured environment values are not echoed to tool output; startup diagnostic stderr is bounded and redacted. Installing a server definition is not proof that its executable or returned instructions are trustworthy.

Do not confuse this client with [Agent Client Protocol](acp.md), which lets an editor control rpi, or the [JSONL control protocol](../user-guide/rpc-json.md).
