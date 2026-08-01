# RPC JSONL protocol

`rpi --mode rpc` (and the dedicated `pi-rpc` binary) expose a long-lived,
LF-delimited JSONL control protocol on stdin/stdout. The protocol is not
JSON-RPC 2.0: every line is a single self-describing JSON object with a `type`
discriminator, and every command can carry an optional `id` that the matching
response echoes.

## Launching the RPC mode

- `rpi --mode rpc [<other rpi flags>]` starts the RPC server on stdin/stdout.
- `pi-rpc` is a thin wrapper (`crates/pi-cli/src/bin/pi-rpc.rs`) that forces
  `--mode rpc` after the caller's arguments, so the binary always enters RPC
  mode even if a conflicting `--mode` is passed. On fatal initialization error
  it writes a failure response line to stdout and exits with status `1`.

Both forms are headless and never prompt for project trust; pass `--approve`
when you need project-local `.pi` resources.

## Framing

- Every record is exactly one JSON object followed by a single line feed (`\n`,
  byte `0x0A`). There is no batching, length prefix, or JSON-RPC envelope.
- The writer serializes each record with `serde_json::to_writer`, appends `\n`,
  and flushes, so clients can read line-by-line.
- The reader splits incoming bytes on `\n`. A trailing carriage return (`\r`) is
  stripped before parsing, so `\r\n` sources are tolerated.
- If stdin reaches EOF while the last partial line is non-empty, that partial
  line is delivered as the final frame.

## Request envelope

Every command line is a JSON object containing at minimum:

```json
{"type": "<command>", "id": "optional-correlation-id", ...}
```

- `type` — required string, snake-cased command name (see the table below).
- `id` — optional string. If present it is echoed in the matching response.
  Commands that do not need correlation may omit it.

## Response envelope

A response line has the fixed shape:

```json
{
  "id": "optional-correlation-id",
  "type": "response",
  "command": "<command>",
  "success": true,
  "data": { ... }
}
```

On failure:

```json
{
  "id": "optional-correlation-id",
  "type": "response",
  "command": "<command>",
  "success": false,
  "error": "human-readable message"
}
```

- `id` matches the request `id` when one was provided; otherwise it is omitted.
- `command` repeats the request's `type` value.
- `data` is present on success and is `null` for commands that return no
  payload.
- `error` is present on failure.

## Correlation and asynchronous events

Command/response pairs correlate by `id`. In addition to responses, the host
emits asynchronous `ApplicationEvent` records on the same stdout stream. Events
have their own `type` tag (e.g. `session_started`, `agent_start`, `agent_settled`,
`todo_updated`, `process_started`) and no `id`. A client should therefore read
all stdout lines, dispatch `"response"` records by `id`, and dispatch events by
`type`.

A minimal exchange looks like:

```jsonl
{"type":"session_started","data":{"version":3,"id":"...","timestamp":"...","cwd":"<workspace>"}}
{"type":"prompt","id":"1","message":"List Rust files"}
{"id":"1","type":"response","command":"prompt","success":true,"data":null}
{"type":"agent_settled"}
```

## Malformed input and recovery

The RPC loop never aborts because of a bad line; it emits a failure response and
continues reading.

- Invalid JSON → `{"type":"response","command":"parse","success":false,"error":"Failed to parse command: ..."}`. Because `serde_json::from_slice` fails before the object is inspected, an invalid JSON line cannot preserve an `id`.
- Missing `type` field → failure response with the preserved `id`.
- Unknown `type` value → `{"id":"...","type":"response","command":"<type>","success":false,"error":"Unknown command: <type>"}`.
- Invalid fields for a known command → failure response with `command` set to the command name and `id` preserved.
- Invalid `extension_ui_response` → failure response with `"command": "extension_ui_response"`.

Because lines are split before parsing, a malformed line never corrupts the
following valid lines.

## Extension UI requests and responses

When an extension asks to show UI, the host emits an `extension_ui_request`
record with a unique `id` and a flattened method object:

```jsonl
{
  "type": "extension_ui_request",
  "id": "ui-1",
  "method": "confirm",
  "title": "Approve destructive edit?",
  "message": "This will overwrite src/main.rs"
}
```

Supported methods include `select`, `confirm`, `input`, `editor`, `notify`,
`setStatus`, `setWidget`, `setTitle`, and `set_editor_text`. The client replies
with an `extension_ui_response` carrying the same `id` and one of:

```jsonl
{"type":"extension_ui_response","id":"ui-1","confirmed":true}
{"type":"extension_ui_response","id":"ui-1","value":"typed answer"}
{"type":"extension_ui_response","id":"ui-1","cancelled":true}
```

Notification/status/widget requests are generated with a host-assigned id and
are one-way; the client does not need to reply to them.

## Stdout isolation

All protocol output goes through a single mutex-protected stdout writer; every
record is produced by the same JSONL write path. No ordinary command handler,
event path, or tool prints raw text to stdout. Runtime diagnostics and panics
are routed to stderr, so stdout remains exclusively JSONL protocol records.

## Command reference

The following table lists every current `RpcCommand` variant exactly once. All
commands accept an optional `id`. Required fields are shown without brackets;
optional fields are shown with `[...]` and their JSON key.

| `type` | Fields | Example request line |
|--------|--------|----------------------|
| `prompt` | `message`; `[images]` array of `ContentBlock`; `[streamingBehavior]` `"steer"` or `"followUp"` | `{"type":"prompt","message":"List Rust files"}` |
| `steer` | `message`; `[images]` | `{"type":"steer","message":"Use async rust"}` |
| `follow_up` | `message`; `[images]` | `{"type":"follow_up","message":"Any tests?"}` |
| `abort` | — | `{"type":"abort"}` |
| `new_session` | `[parentSession]` path string | `{"type":"new_session","parentSession":"<workspace>/parent.jsonl"}` |
| `get_state` | — | `{"type":"get_state"}` |
| `set_model` | `provider`, `modelId` | `{"type":"set_model","provider":"anthropic","modelId":"claude-sonnet-4-5"}` |
| `cycle_model` | — | `{"type":"cycle_model"}` |
| `get_available_models` | — | `{"type":"get_available_models"}` |
| `set_thinking_level` | `level` (`"off"`, `"minimal"`, `"low"`, `"medium"`, `"high"`, `"xhigh"`) | `{"type":"set_thinking_level","level":"high"}` |
| `cycle_thinking_level` | — | `{"type":"cycle_thinking_level"}` |
| `get_available_thinking_levels` | — | `{"type":"get_available_thinking_levels"}` |
| `set_steering_mode` | `mode` (`"all"`, `"one-at-a-time"`) | `{"type":"set_steering_mode","mode":"all"}` |
| `set_follow_up_mode` | `mode` (`"all"`, `"one-at-a-time"`) | `{"type":"set_follow_up_mode","mode":"one-at-a-time"}` |
| `compact` | `[customInstructions]` | `{"type":"compact","customInstructions":"Summarize recent context"}` |
| `set_auto_compaction` | `enabled` boolean | `{"type":"set_auto_compaction","enabled":true}` |
| `set_auto_retry` | `enabled` boolean | `{"type":"set_auto_retry","enabled":true}` |
| `abort_retry` | — | `{"type":"abort_retry"}` |
| `bash` | `command`; `[excludeFromContext]` boolean | `{"type":"bash","command":"pwd","excludeFromContext":true}` |
| `abort_bash` | — | `{"type":"abort_bash"}` |
| `get_session_stats` | — | `{"type":"get_session_stats"}` |
| `export_html` | `[outputPath]` | `{"type":"export_html","outputPath":"<workspace>/out.html"}` |
| `switch_session` | `sessionPath` | `{"type":"switch_session","sessionPath":"<workspace>/session.jsonl"}` |
| `fork` | `entryId` | `{"type":"fork","entryId":"entry-1"}` |
| `clone` | — | `{"type":"clone"}` |
| `get_fork_messages` | — | `{"type":"get_fork_messages"}` |
| `get_entries` | `[since]` entry id | `{"type":"get_entries","since":"entry-1"}` |
| `get_tree` | — | `{"type":"get_tree"}` |
| `get_last_assistant_text` | — | `{"type":"get_last_assistant_text"}` |
| `set_session_name` | `name` | `{"type":"set_session_name","name":"demo"}` |
| `get_messages` | — | `{"type":"get_messages"}` |
| `get_commands` | — | `{"type":"get_commands"}` |
| `set_todos` | `phases` array of `TodoPhase` | `{"type":"set_todos","phases":[]}` |
| `loop_create` | `interval`, `prompt`; `[fireImmediately]` default `true`; `[durable]` default `false` | `{"type":"loop_create","interval":"5m","prompt":"check","fireImmediately":true,"durable":false}` |
| `loop_update` | `taskId`; `[interval]`; `[prompt]` | `{"type":"loop_update","taskId":"loop-1","interval":"10m","prompt":"check again"}` |
| `loop_list` | — | `{"type":"loop_list"}` |
| `loop_delete` | `taskId` | `{"type":"loop_delete","taskId":"loop-1"}` |
| `loop_cancel` | `taskId` | `{"type":"loop_cancel","taskId":"loop-1"}` |
| `process_spawn` | `spec` (`argv`, `cwd`, `[env]`, `[tty]`, `[terminalSize]`, `[label]`, `[timeoutMs]`, `[outputBytes]`) | `{"type":"process_spawn","spec":{"argv":["printf","ok"],"cwd":"<workspace>","env":{},"tty":false}}` |
| `process_list` | — | `{"type":"process_list"}` |
| `process_describe` | `processId` | `{"type":"process_describe","processId":"00000000-0000-7000-8000-000000000000"}` |
| `process_logs` | `processId`; `[cursor]` default `0`; `[limitBytes]` | `{"type":"process_logs","processId":"00000000-0000-7000-8000-000000000000","cursor":0,"limitBytes":1024}` |
| `process_write` | `processId`, `dataBase64` | `{"type":"process_write","processId":"00000000-0000-7000-8000-000000000000","dataBase64":"b2s="}` |
| `process_keys` | `processId`, `keys` (`"ENTER"`, `"TAB"`, `"ESCAPE"`, `"CTRL_C"`, `"CTRL_D"`, `"UP"`, `"DOWN"`, `"LEFT"`, `"RIGHT"`) | `{"type":"process_keys","processId":"00000000-0000-7000-8000-000000000000","keys":["ENTER","CTRL_C"]}` |
| `process_resize` | `processId`, `cols`, `rows` | `{"type":"process_resize","processId":"00000000-0000-7000-8000-000000000000","cols":80,"rows":24}` |
| `process_signal` | `processId`, `signal` (`"SIGINT"`, `"SIGTERM"`, `"SIGHUP"`, `"SIGQUIT"`, `"SIGKILL"`) | `{"type":"process_signal","processId":"00000000-0000-7000-8000-000000000000","signal":"SIGTERM"}` |
| `process_stop` | `processId` | `{"type":"process_stop","processId":"00000000-0000-7000-8000-000000000000"}` |
| `process_wait` | `processId`; `[timeoutMs]` | `{"type":"process_wait","processId":"00000000-0000-7000-8000-000000000000","timeoutMs":500}` |

### Common payload notes

- `images` items are `ContentBlock` objects; an inline image looks like
  `{"type":"image","data":"<base64>","mimeType":"image/png"}`.
- `QueueMode` values serialize as kebab-case: `"all"` and `"one-at-a-time"`.
- `ThinkingLevel` values are lowercase: `"off"`, `"minimal"`, `"low"`,
  `"medium"`, `"high"`, `"xhigh"`.
- `ProcessSignal` values serialize as `SCREAMING_SNAKE_CASE`.
- `ProcessKey` values serialize as the variant names shown above.
- `ProcessId` is a string (UUID v7 in generated ids), not a number.

## Example RPC session

```jsonl
{"type":"prompt","id":"1","message":"List Rust files"}
{"type":"get_state","id":"2"}
{"id":"1","type":"response","command":"prompt","success":true,"data":null}
{"id":"2","type":"response","command":"get_state","success":true,"data":{"thinkingLevel":"medium","isStreaming":false,"sessionName":"demo"}}
{"type":"agent_settled"}
{"type":"bash","id":"3","command":"ls *.rs"}
{"id":"3","type":"response","command":"bash","success":true,"data":{"exitCode":0,"stdout":"main.rs\nlib.rs\n"}}
```

For a complete Rust client example, see
[`examples/src/bin/rpc_client.rs`](../examples/src/bin/rpc_client.rs).
