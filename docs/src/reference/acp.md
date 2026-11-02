# Agent Client Protocol (ACP) mode

Agent Client Protocol lets an editor launch and control rpi through JSON-RPC 2.0. rpi implements protocol version 1. This is a different transport and method set from `rpi rpc`.

## Transports

```sh
rpi agent stdio
rpi agent serve --address 127.0.0.1:34567
```

Stdio uses Content-Length framing. Serve uses WebSocket text frames, without Content-Length headers, and is **loopback-only**. Its optional `--token-file` gates connections through a bearer header or `rpi-auth.<token>` subprotocol. Tokenless loopback accepts native clients but rejects browser-origin connections. This endpoint does not inherit the standalone Web listener's TLS feature set.

## Methods

| Method | Behavior |
| --- | --- |
| `initialize` | Negotiate protocol, capabilities, and authentication methods |
| `authenticate` | Acknowledge the configured `rpi-auth` method |
| `session/new` | Create an independent application with client-supplied working directory |
| `session/prompt` | Run one turn and return its stop reason |
| `session/cancel` | Notification that stops the active turn |
| `session/close` | Stop work and release a session |
| `logout` | Clear authenticated state |

The agent streams `session/update` notifications for user/assistant text, thought text, tool calls, tool-call updates, and usage. Tool approvals use reverse `session/request_permission` requests with allow-once/reject-once decisions. Permission requests time out after ten minutes.

One prompt can run per session at a time; another prompt on the same session is rejected. Different sessions run concurrently. The protocol session handle identifies its independent application, while native conversation persistence uses the normal rpi session store unless disabled.

## Explicit limitations

- `mcpServers` supplied with `session/new` are accepted but not connected.
- `session/load`, `session/resume`, `session/list`, and `session/delete` are not advertised.
- Authentication acknowledges existing rpi credentials; it is not a flow for sending provider secrets through editor messages.
- The WebSocket server does not offer remote plaintext access or TLS termination.

Frames are bounded to 16 MiB, and embedded resource-link content has a separate 2 MiB cap. Client input remains subject to filesystem and tool authority.

## Security

Use configured provider credentials and protect any transport token. Do not interpret a protocol handshake as approval to run every tool. The reverse permission gate feeds the same host approval path as other interfaces.
