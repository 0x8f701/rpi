# Extensions and process protocol

`rpi` supports long-lived process extensions over the strict versioned LF JSONL
protocol. A manifest can launch an existing executable, or run a JavaScript
entry through the **in-process QuickJS runtime** (`.js`/`.mjs` only). QuickJS
is embedded in `rpi`; no external JavaScript runtime is required. The in-process
runtime speaks the same extension protocol over in-memory channels, so
executable and QuickJS extensions share the same host machinery (handshake,
load, invocation, cancellation, shutdown).

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

The equivalent explicit process form adds `"runtime": "process"`. A QuickJS
extension instead uses the discriminated `runtime`/`entry` form:

```json
{
  "schemaVersion": 1,
  "id": "pi-weather",
  "runtime": "quickjs",
  "entry": "./index.mjs",
  "capabilities": ["commands", "tools", "event_hooks", "session_actions", "ui"],
  "uiCapabilities": ["notify", "status"]
}
```

Fields:

- `schemaVersion` — must be `1`.
- `id` — alphanumeric plus `_`, `-`, or `.`.
- `runtime` — optional `process` for executables, or required `quickjs` for
  script entries. Omitting it preserves the original executable manifest shape.
- `executable` — process-only path relative to the manifest directory.
- `arguments` — optional process-only argv; QuickJS manifests reject it.
- `entry` — QuickJS-only contained relative `.js` or `.mjs` file. TypeScript
  (`.ts`) is not supported by the in-process runtime.
- `capabilities` — one or more of `commands`, `tools`, `event_hooks`,
  `message_renderers`, `provider_metadata`, `session_actions`, `ui`,
  `overlays`. QuickJS entries reject `message_renderers` and
  `provider_metadata` because their factories cannot cross the extension
  protocol.
- `uiCapabilities` — required when `ui` is listed; one or more of `select`,
  `confirm`, `input`, `editor`, `notify`, `status`, `widget`, `title`,
  `set_editor_text`, `overlay`.

The manifest is validated before launch. Absolute paths, parent traversal,
symlink escapes, missing files, mixed `executable`/`entry` fields, unsupported
script extensions, and untrusted project manifests fail closed.

## QuickJS extension API

The entry must default-export an OMP-style factory:

```js
export default function (pi) {
  pi.registerCommand("hello", {
    description: "Say hello",
    handler: async (_args, ctx) => {
      ctx.ui.notify("Hello from QuickJS", "info");
    },
  });
}
```

The in-process runtime supports the full protocol-backed surface:

- the 33 lifecycle hook names documented by the OMP ExtensionAPI
- `pi.registerCommand`, `pi.registerTool`, and the in-process `pi.events` bus
- invocation context metadata such as mode, cwd, trust, cancellation, and
  model/thinking state when supplied by the Rust host
- synchronous snapshot getters `getSessionName`, `getThinkingLevel`,
  `getActiveTools`, `getAllTools`, and `getCommands`
- with `session_actions`, `sendMessage`, `sendUserMessage`, `appendEntry`,
  `setSessionName`, `setLabel`, `setActiveTools`, `setModel`, and
  `setThinkingLevel`, plus context `abort`, `shutdown`, `compact`, `reload`, and
  `waitForIdle`
- UI methods `select`, `confirm`, `input`, `editor`, `notify`, `setStatus`,
  `setWidget` with string arrays, `setTitle`, `setEditorText`, plus the
  query methods `getEditorText`, `getAllThemes`, `getTheme`, `setTheme`, and
  `getToolsExpanded`

Handlers receive `AbortSignal` cancellation for tools. Tool updates,
cancellation, shutdown, UI requests, and hook return values translate onto the
same protocol used by executable extensions.

Sandbox guarantees:

- Each QuickJS extension runs in its own dedicated OS thread with its own
  runtime/context, a 64 MiB memory limit, and a host-timeout interrupt handler
  that bounds runaway bytecode.
- The runtime exposes no `process`, `require`, `fetch`, `console`, or other
  Node/Bun-style host globals; extension code has no process, filesystem, or
  network access. Any host capability must go through the `pi` API.
- Outbound frames (responses, registrations, updates, actions) are bounded by
  `max_frame_bytes` like every process extension.

See [`examples/quickjs_extension.mjs`](../../../examples/quickjs_extension.mjs) for
commands, a tool, an event hook, session actions, and UI actions.

## Packaging

Extensions are distributed through rpi packages (local or git). Place the
manifest and executable in a package under `extensions/` and reference the
package in `settings.json`:

```json
{
  "packages": [
    { "source": "git:github.com/owner/pi-weather", "extensions": ["pi-weather"] }
  ]
}
```

Project extensions are only loaded when the project is trusted.

## Protocol overview

The host spawns the extension with environment variables:

- `PI_EXTENSION_PROTOCOL_VERSION=1`
- `PI_EXTENSION_ID=<id>`
- `PI_EXTENSION_PACKAGE_ID=<package-id>`
- `PI_EXTENSION_ENTRY=<canonical-entry>` (QuickJS only)
- `PI_EXTENSION_CAPABILITIES=<JSON-array>` (QuickJS only)
- `PI_EXTENSION_UI_CAPABILITIES=<JSON-array>` (QuickJS only)
- `PI_EXTENSION_MAX_FRAME_BYTES=<bytes>` (QuickJS only)

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
| `overlays` | Register `pi.registerOverlay` overlays (rows + optional interactive input) |

## Custom tool fallback

If you only need a one-off custom tool and don't want to write a full
extension, library users can register additional `pi_agent::AgentTool`s when
constructing a `pi_coding::Session`. See
[`examples/src/bin/custom_tool.rs`](../../../examples/src/bin/custom_tool.rs).

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
  `PI_EXTENSION_*` protocol variables.
- Process executables must be regular files and executable on Unix.
- Process executable and QuickJS entry paths are resolved relative to the
  manifest and must remain inside the package directory.
- QuickJS entries run only from explicit trusted manifests inside the
  in-process sandbox: a dedicated thread, a 64 MiB memory limit, an interrupt
  deadline, and no `process`/`require`/`fetch`/`console` globals.
- On Unix the child runs in its own process group (`process_group(0)`); on
  termination the host sends `SIGKILL` to the entire group so descendant
  processes are cleaned up, then falls back to killing the immediate child.
- Every JSONL frame is bounded by `max_frame_bytes` (default 1 MiB); frames
  that exceed the limit, blank lines, and CRLF terminators are rejected.
- QuickJS entries reject the `message_renderers` and `provider_metadata`
  capabilities because their factories cannot cross the protocol boundary.
- Extension-initiated runtime actions, including `reload`, are unavailable
  during the registration-only load phase and can be rejected when the
  `session_actions` capability is missing or the host cannot process the
  action.
- Project extensions are only launched when the project is trusted.
- `kill_on_drop` and runtime invalidation terminate process children; QuickJS
  instances are joined and torn down on shutdown.
