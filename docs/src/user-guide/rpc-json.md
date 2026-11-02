# RPC JSONL protocol

Start `rpi rpc` or `rpi --mode rpc`. Send one JSON object per line on standard input and parse one JSON record per line on standard output. This protocol is not JSON-RPC 2.0 and does not use Content-Length framing. The separate [Agent Client Protocol](../reference/acp.md) does.

## Commands and responses

Commands use a snake-case `type` discriminator and an optional string `id`. Most compound fields use camelCase. Correlate responses by `id`; do not assume that the next output line is the response, because application events also use the stream.

```json
{"type":"get_state","id":"state-request"}
{"type":"prompt","id":"prompt-request","message":"Explain the project structure"}
{"type":"abort","id":"stop-request"}
```

A response contains `type: "response"`, `command`, and `success`. It includes `id` when provided, optional `sessionId`, optional `data`, and optional `error`. A successful command response is not by itself proof that a streamed model turn has finished.

## Conversation and model control

| Command | Fields in addition to `type` and optional `id` |
| --- | --- |
| `prompt` | Required `message`; optional `images`, `streamingBehavior` |
| `steer`, `follow_up` | Required `message`; optional `images` |
| `abort` | None; interrupts generation |
| `get_state` | None |
| `set_model` | Required `provider`, `modelId` |
| `cycle_model`, `get_available_models` | None |
| `set_thinking_level` | Required `level` |
| `cycle_thinking_level`, `get_available_thinking_levels` | None |
| `compact` | Optional `customInstructions` |
| `set_auto_compaction`, `set_auto_retry` | Required `enabled` boolean |
| `abort_retry` | None |
| `bash` | Required `command`; optional `excludeFromContext` |
| `abort_bash` | None |
| `get_session_stats` | None |

The UI labels interruption **Stop**; the wire command remains `abort`.

**Steering queue status: in progress.** The Web component displays queued steering previews. Rust queue cancellation is still being integrated. `queue_cancel` has evolving scope/index behavior; do not treat `all`, `steering`, and indexed cancellation as a stable end-to-end contract until that integration is complete.

## Session control

An explicit absolute `cwd` for `new_session` is created when missing, including nested directories, through capability-walked no-follow creation. Symlinks, malformed paths, and file conflicts fail closed. An omitted or empty `cwd` preserves source-session inheritance. This differs from CLI `-C`, which requires an existing directory.

`new_session` also accepts optional `parentSession`.

`session_list` accepts `scope: "current"` (the default) or `"all_projects"`. It returns the unified native/foreign resume catalog under the configured source policy.

`switch_session` requires `sessionPath`. The Web handoff form opts into `responseMode: "subscribe-handoff-v1"`. Its `catalogSource`, `catalogSessionId`, and `catalogStatus` fields must be supplied together when present. The server validates that catalog identity before import or runtime creation. A foreign import obtains a distinct native session identity.

`export_html` accepts optional `outputPath` and `sessionPath`. See [export and share](../reference/export-share.md).

## Recurring loops

```json
{"type":"loop_create","id":"loop-request","interval":"5m","prompt":"Check the current build status","fireImmediately":true,"durable":false}
{"type":"loop_list","id":"loops-request"}
```

`loop_create` requires string `interval` and `prompt`. `fireImmediately` defaults to `true`; `durable` defaults to `false`. Intervals are positive bare seconds or an integer followed by `s`, `m`, `h`, or `d`.

`loop_delete` requires `taskId` returned by creation/listing. `loop_update` and `loop_cancel` are also supported; deletion and cancelling an execution are separate operations. The Web panel exposes presets 30s, 1m, 5m, 15m, 1h, and 4h.

## Supervised processes

`process_spawn` takes a `spec` object:

| Field | Type | Required/default |
| --- | --- | --- |
| `argv` | Array of strings | Required executable and arguments |
| `cwd` | String path | Required working directory |
| `env` | Object of string or null values | Empty object |
| `tty` | Boolean | `false` |
| `terminalSize` | Object with integer `rows`, `cols` | Optional |
| `label` | String | Optional |
| `timeoutMs` | Unsigned integer | Optional |
| `outputBytes` | Unsigned integer | Optional |

The Web panel supplies `argv: ["bash", "-c", command]` with its session working directory; the protocol itself accepts an argument vector.

| Command | Additional fields |
| --- | --- |
| `process_list` | None |
| `process_describe` | `processId` |
| `process_logs` | `processId`; optional `cursor`, `limitBytes` |
| `process_write` | `processId`, `dataBase64` |
| `process_keys` | `processId`, `keys` array |
| `process_resize` | `processId`, `rows`, `cols` |
| `process_signal` | `processId`, `signal` |
| `process_stop` | `processId` |
| `process_wait` | `processId`; optional `timeoutMs` |

Signals are `SIGINT`, `SIGTERM`, `SIGHUP`, `SIGQUIT`, and `SIGKILL`. Process states are `starting`, `running`, `stopping`, `exited`, `timed_out`, `expired`, and `failed`.

Log chunks contain base64 bytes, not independently decoded text. Preserve byte order when decoding UTF-8 and observe reported cursor/output loss. `lost` and `lostBytes` identify output no longer available. The Web viewer requests 16 KiB; that UI size is not the protocol's general log-read default.

## Transport and safety

The Web listener carries application commands through HTTP/WebSocket with listener authentication and session routing. Preserve session identity on multiplexed traffic; never apply an event from one session to another transcript. A disconnected view does not establish that the session's work stopped.

Tool execution remains subject to host approval and sandbox policy. Treat RPC access and pairing URLs as privileged access to the running agent. See [Web client](web.md) for listener flags and token pairing.
