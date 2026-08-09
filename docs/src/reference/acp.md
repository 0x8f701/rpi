# Agent Client Protocol (ACP) mode

`rpi` implements the stable **Agent Client Protocol v1** (ACP,
agentclientprotocol.com) — a JSON-RPC 2.0 protocol that lets ACP-speaking
editors embed coding agents. It is exposed as `rpi agent stdio` and
`rpi agent serve` (`crates/pi-cli/src/modes/acp.rs`).

## Transports

- **stdio** — `rpi agent stdio` speaks Content-Length framed JSON-RPC 2.0 on
  stdin/stdout (the same framing as the LSP server), so an editor spawns it
  the way it spawns a language server.
- **serve** — `rpi agent serve` speaks the same messages as WebSocket text
  frames (frames capped at 16 MiB). The server is loopback-only (plaintext
  WebSocket cannot safely carry the bearer token off the local host; TLS is
  tracked for a later release).

## Methods

Client → Agent requests:

| Method | Purpose |
|--------|---------|
| `initialize` | Negotiate the protocol version and capabilities; advertises the `rpi-auth` auth method. |
| `authenticate` | Acknowledge rpi's configured-credential auth (`methodId: "rpi-auth"`). rpi never collects secrets over ACP — credentials come from `auth.json`/provider env keys, and `session/new` performs the real gate (fails with `auth_required` when no authenticated model exists). |
| `session/new` | Create a session rooted at a client-supplied absolute `cwd`; returns `sess_<uuid>`. Each session builds an independent rpi `Application` and records to the normal session store unless `--no-session`, so resumed rpi runs see the same conversation. |
| `session/prompt` | Run a turn; the assistant response streams back as `session/update` notifications and the request resolves with a `stopReason`. |
| `session/cancel` | Abort the active turn (request or notification form); the pending `session/prompt` resolves with `stopReason: "cancelled"`. |
| `session/close` | Cancel ongoing work and release the session. |
| `logout` | Acknowledge (rpi's credential state is process-wide). |

Agent → Client **reverse requests**:

- `session/request_permission` — the tool-approval gate. When the session's
  approval mode (`--approval-mode` flag or the `approvalMode` setting)
  requires confirmation, the agent asks the client for an `allow-once` /
  `reject-once` decision before the tool executes. Evaluation order:
  path-level `permissionRules` first (same evaluation as the interactive
  host hook), then the capability-wide approval mode, then the ACP reverse
  round trip (`acp_approval_before_tool_call` in `acp.rs:330-364`). The
  decision feeds the tool call; a timeout (600 s) or client disconnect
  blocks the tool with an actionable message.

Agent → Client **notifications**:

- `session/update` — `user_message_chunk`, `agent_message_chunk`,
  `agent_thought_chunk`, `tool_call`, `tool_call_update`, and
  `usage_update` variants, projected from rpi's `ApplicationEvent`s.

## Prompt content

`session/prompt` accepts a `ContentBlock[]` array: baseline `text` and
`resource_link` (file contents embedded up to 2 MiB), plus `image` and
`resource` blocks (advertised via `promptCapabilities`). An empty prompt is
rejected with `invalid_params`.

## Capabilities advertised

`initialize` returns `agentCapabilities` with `loadSession: false`,
`promptCapabilities: { image: true, audio: false, embeddedContext: true }`,
`sessionCapabilities: { close: {} }`, and `auth: { logout: {} }`.
`session/load`, `session/resume`, `session/list`, and `session/delete` are
not advertised.

## Concurrency and isolation

- One prompt turn per session at a time; concurrent prompts on the same
  session fail with an actionable error. Different sessions run
  concurrently, each with its own permission-bridge session id (concurrent
  turns never cross permission requests).
- Cancelling a prompt turn resolves its pending permission requests with the
  `cancelled` outcome.
- `mcpServers` in `session/new` are accepted but not connected yet
  (documented limitation).

## Example

```json
{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1}}
{"jsonrpc":"2.0","id":2,"method":"session/new","params":{"cwd":"/path/to/project"}}
{"jsonrpc":"2.0","id":3,"method":"session/prompt","params":{"sessionId":"sess_…","prompt":[{"type":"text","text":"List the Rust files"}]}}
```

## Related documentation

- [`rpc-json.md`](../user-guide/rpc-json.md) — the rpi-native JSONL control
  plane (different protocol, same Application runtime)
- [`security.md`](security.md) — approval modes and permission rules
- [`settings-trust.md`](settings-trust.md) — `approvalMode`,
  `permissionRules`
