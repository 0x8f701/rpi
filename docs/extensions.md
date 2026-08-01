# Extensions and process protocol

`pi` supports long-lived process extensions over the strict versioned LF JSONL
protocol. A manifest can launch either an existing executable or an optional
Bun-hosted TypeScript/JavaScript entry through Pi's bundled bridge. Bun remains
an external child process; Pi does not embed a JavaScript runtime.

## Manifest (`pi-extension.json`)

Every extension needs an explicit manifest. Existing executable manifests stay
valid unchanged:

```json
{
  "schemaVersion": 1,
  "id": "pi-weather",
  "executable": "./main",
  "arguments": [],
  "capabilities": ["tools"],
  "uiCapabilities": []
}
```

The equivalent explicit process form adds `"runtime": "process"`. A Bun
extension instead uses the discriminated `runtime`/`entry` form:

```json
{
  "schemaVersion": 1,
  "id": "pi-weather",
  "runtime": "bun",
  "entry": "./index.ts",
  "capabilities": ["commands", "tools", "event_hooks", "session_actions", "ui"],
  "uiCapabilities": ["notify", "status"]
}
```

Fields:

- `schemaVersion` — must be `1`.
- `id` — alphanumeric plus `_`, `-`, or `.`.
- `runtime` — optional `process` for executables, or required `bun` for script
  entries. Omitting it preserves the original executable manifest shape.
- `executable` — process-only path relative to the manifest directory.
- `arguments` — optional process-only argv; Bun manifests reject it.
- `entry` — Bun-only contained relative `.ts`, `.js`, `.mjs`, or `.cjs` file.
- `capabilities` — one or more of `commands`, `tools`, `event_hooks`,
  `message_renderers`, `provider_metadata`, `session_actions`, `ui`. Bun entries
  reject `message_renderers` and `provider_metadata` because their factories
  cannot cross the process boundary.
- `uiCapabilities` — required when `ui` is listed; one or more of `select`,
  `confirm`, `input`, `editor`, `notify`, `status`, `widget`, `title`,
  `set_editor_text`.

The manifest is validated before launch. Absolute paths, parent traversal,
symlink escapes, missing files, mixed `executable`/`entry` fields, unsupported
script extensions, and untrusted project manifests fail closed.

## Bun extension API

The bundled bridge imports the entry's default export as an OMP-style factory:

```ts
export default function (pi: any) {
  pi.registerCommand("hello", {
    description: "Say hello",
    handler: async (_args: string, ctx: any) => {
      ctx.ui.notify("Hello from Bun", "info");
    },
  });
}
```

The constrained bridge supports the existing protocol-backed surface:

- the 33 lifecycle hook names documented by the OMP ExtensionAPI
- `pi.registerCommand`, `pi.registerTool`, and the in-process `pi.events` bus
- invocation context metadata such as mode, cwd, trust, cancellation, session
  identity/name, idle state, model/thinking state, usage, and system prompt when
  supplied by the Rust host
- synchronous snapshot getters `getSessionName`, `getThinkingLevel`,
  `getActiveTools`, `getAllTools`, and `getCommands`
- with `session_actions`, `sendMessage`, `sendUserMessage`, `appendEntry`,
  `setSessionName`, `setLabel`, `setActiveTools`, `setModel`, and
  `setThinkingLevel`, plus context `abort`, `shutdown`, `compact`, `reload`, and
  `waitForIdle`
- UI methods `select`, `confirm`, `input`, `editor`, `notify`, `setStatus`,
  `setWidget` with string arrays, `setTitle`, and `setEditorText`

Handlers receive `AbortSignal` cancellation for tools. Tool updates,
cancellation, shutdown, UI requests, and hook return values translate onto the
same protocol used by executable extensions. Console/stdout noise is redirected
to sanitized stderr so stdout remains protocol-only. APIs requiring in-process
TUI component or theme objects fail explicitly with an actionable
`unavailable in the process-hosted ExtensionAPI` error.

Bun is resolved directly as argv without shell interpolation. Set
`PI_BUN_EXECUTABLE` to an absolute Bun executable when it is not on `PATH`.
The bridge source is bundled into `pi` and materialized independently of the
installed package location.

See [`examples/bun_extension.ts`](../examples/bun_extension.ts) for commands,
a tool, an event hook, session actions, and UI actions.

## Packaging

Extensions are distributed through Pi packages (local or git). Place the
manifest and executable in a package under `extensions/` and reference the
package in `settings.json`:

```json
{
  "packages": [
    { "source": "git:owner/pi-weather", "extensions": ["pi-weather"] }
  ]
}
```

Project extensions are only loaded when the project is trusted.

## Protocol overview

The host spawns the extension with environment variables:

- `PI_EXTENSION_PROTOCOL_VERSION=1`
- `PI_EXTENSION_ID=<id>`
- `PI_EXTENSION_PACKAGE_ID=<package-id>`
- Bun only: `PI_EXTENSION_ENTRY=<canonical-entry>`
- Bun only: `PI_EXTENSION_CAPABILITIES=<JSON-array>`
- Bun only: `PI_EXTENSION_UI_CAPABILITIES=<JSON-array>`
- Bun only: `PI_EXTENSION_MAX_FRAME_BYTES=<bytes>`

The extension reads JSON frames from stdin and writes JSON frames to stdout.

### Host frames (host → extension)

```json
{
  "type": "hello",
  "protocolVersion": 1,
  "instance": { "extensionId": "pi-weather", "generation": 1 },
  "cwd": "<workspace>",
  "mode": "tui",
  "projectTrusted": true
}
```

```json
{
  "type": "request",
  "id": "req-1",
  "generation": 1,
  "request": { "kind": "initialize" }
}
```

```json
{
  "type": "request",
  "id": "req-2",
  "generation": 1,
  "request": {
    "kind": "invoke",
    "invocation": {
      "kind": "tool",
      "name": "weather",
      "callId": "call-1",
      "arguments": { "city": "London" }
    }
  }
}
```

```json
{
  "type": "shutdown",
  "reason": "session ended"
}
```

### Extension frames (extension → host)

```json
{
  "type": "hello",
  "protocolVersion": 1,
  "manifest": {
    "id": "pi-weather",
    "name": "Weather",
    "version": "0.1.0",
    "capabilities": ["tools"],
    "uiCapabilities": []
  }
}
```

```json
{
  "type": "register",
  "registration": {
    "kind": "tool",
    "tool": {
      "name": "weather",
      "label": "Weather",
      "description": "Get current weather",
      "parameters": {
        "type": "object",
        "properties": { "city": { "type": "string" } },
        "required": ["city"]
      }
    }
  }
}
```

```json
{
  "type": "response",
  "id": "req-2",
  "result": {
    "status": "success",
    "value": {
      "content": [
        { "type": "text", "text": "15 °C, cloudy" }
      ]
    }
  }
}
```

Tool results use the same shape as `AgentToolResult`.

## Capabilities

| Capability | What the extension can do |
|------------|---------------------------|
| `tools` | Register tools the agent can call |
| `commands` | Register slash commands |
| `event_hooks` | Receive agent lifecycle events |
| `message_renderers` | Render custom message types |
| `provider_metadata` | Provide extra model metadata |
| `session_actions` | Read invocation snapshots and request session/model/tool actions |
| `ui` | Show select/confirm/input/editor/status widgets |

## Custom tool fallback

If you only need a one-off custom tool and don't want to write a full
extension, library users can register additional `pi_agent::AgentTool`s when
constructing a `pi_coding::Session`. See
[`examples/src/bin/custom_tool.rs`](../examples/src/bin/custom_tool.rs).

## Timeout defaults

The host uses these timeouts unless overridden by the extension launcher:

- Handshake: 5s
- Load: 10s
- Initialize: 15s
- Invocation: 60s
- Hook: 10s
- Shutdown: 2s

## Security

- The child process starts with `env_clear()` and receives only the
  explicitly declared manifest environment entries plus the required
  `PI_EXTENSION_*` protocol variables. The host removes `PI_BUN_EXECUTABLE`
  from the child's environment before spawn.
- Process executables must be regular files and executable on Unix.
- Process executable and Bun entry paths are resolved relative to the manifest
  and must remain inside the package directory.
- Bun entries run only from explicit trusted manifests through the bundled
  bridge and a separately spawned Bun process.
- On Unix the child runs in its own process group (`process_group(0)`); on
  termination the host sends `SIGKILL` to the entire group so descendant
  processes are cleaned up, then falls back to killing the immediate child.
- Every JSONL frame is bounded by `max_frame_bytes` (default 1 MiB); frames
  that exceed the limit, blank lines, and CRLF terminators are rejected.
- Bun entries reject the `message_renderers` and `provider_metadata`
  capabilities because their factories cannot cross the process boundary.
- Extension-initiated runtime actions, including `reload`, are unavailable
  during the registration-only load phase and can be rejected when the
  `session_actions` capability is missing or the host cannot process the
  action.
- Project extensions are only launched when the project is trusted.
- `kill_on_drop` and runtime invalidation terminate process and Bun children.
