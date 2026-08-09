# Hooks and trust hooks

Host hooks are external commands that observe (and, for two events, gate)
session activity. They are configured under `settings.hooks` and run by the
`HostHooks` runtime (`crates/pi-coding/src/hooks.rs`). Extensions additionally
receive lifecycle events — including a `trust_decision` event that can
recommend approving a tentative project trust decision.

## Host hooks

```json
{
  "hooks": [
    { "event": "pre_tool_call", "matcher": "read", "command": ["/opt/hooks/guard", "--strict"], "timeoutMs": 1200, "failClosed": true },
    { "event": "session_end", "command": ["/opt/hooks/bye"] }
  ]
}
```

`HookConfig` fields (`settings.rs:731-757`):

| Field | Meaning |
|-------|---------|
| `event` | `pre_tool_call`, `post_tool_call`, `session_start`, `session_end`, `turn_start`, `turn_end`, or `pre_trust_decision`. Unknown events are rejected at deserialize time. |
| `matcher` | Exact or substring match on the event subject (tool name, message role, or canonical project path). Absent matchers fire for every subject. |
| `command` | Command argv run without a shell (`command[0]` is the executable, the rest are argv); must be non-empty. |
| `timeoutMs` | Per-hook timeout; defaults to 5000, capped at 60000. A timed-out hook's process group is killed. |
| `enabled` | Set `false` to skip the entry without removing it. |
| `failClosed` | Only meaningful for `pre_tool_call` and `pre_trust_decision`: when the hook errors or times out, fail closed (block the tool / deny the trust decision) instead of the default fail-open (allow). |

Hooks are external commands run **without a shell**. The event payload is
written as JSON on stdin; stdout (capped) is parsed as JSON for
`pre_tool_call` and `pre_trust_decision` decisions. The envelope carries
`event`, `cwd`, `sessionId`, `timestamp`, and `subject` — **no secrets**.

### Blocking semantics

Only `pre_tool_call` and `pre_trust_decision` can block: a
`{"decision":"block","reason":"..."}` response prevents the tool from running
or denies the tentative trust decision. Every other event is advisory
(logged). Hook failures (spawn error, non-zero exit, timeout, malformed JSON)
fail **open** for the two blocking events unless the entry sets
`failClosed: true`, in which case the tool is blocked (or the trust decision
denied) instead.

- `pre_tool_call` payloads include the tool `name` and a text rendering of
  `arguments`.
- `pre_trust_decision` payloads carry the canonical project `path`, the
  tentative `decision` (`trusted`/`untrusted`/`ask`), and `isNew` — the same
  spelling the extension `trust_decision` event uses
  (`hooks.rs:333-351`).

Host approval runs before the existing host hooks and extension reducers; a
denial skips later hooks.

## Extension trust hook

Extensions registered for the `event_hooks` capability receive a
`trust_decision` event (`{path, decision, isNew}`) and may recommend approval
with `{approve: true}` (`ExtensionTrustDecisionReduction` in
`extensions.rs:2187-2196`). The contract is fail-open by design:

- The event never carries a deny surface — an extension can only recommend
  approval.
- The recommendation can only upgrade an undecided (`ask`) tentative decision
  to trusted; the host applies it via
  `crate::trust::apply_trust_hook_outcomes`, so a stored denial is never
  weakened.

Project-trust extensions also receive a `project_trust` event
(`ExtensionProjectTrustReduction`), and untrusted project extension manifests
are refused at load/execute time (`extensions.rs:413-418`, `623-625`).

## Where hooks are applied

- Session lifecycle: `session_start` / `session_end` fire around the session.
- Turn lifecycle: `turn_start` / `turn_end` fire around each agent turn.
- Tool lifecycle: `pre_tool_call` / `post_tool_call` observe (and gate) each
  tool invocation; the post hook observes the final result after extension
  reduction, and its output never mutates the tool result (except for
  doom-loop recovery, which replaces the result with an actionable stop
  message — see [`session-recovery.md`](../user-guide/session-recovery.md)).
- Trust: `pre_trust_decision` fires for a tentative trust decision before
  the stored decision is consulted/recorded.

## Invariants

- Hooks never run through a shell; argv is passed directly.
- Payloads are bounded (stdout capped), redacted (no secrets), and carry a
  millisecond timestamp.
- Blocking is opt-in per event: only `pre_tool_call` and
  `pre_trust_decision` can block, and only with an explicit `failClosed`
  entry do errors turn into blocks.
- Disabled entries never fire; empty commands are skipped with a diagnostic.
- Extension trust recommendations can only upgrade an `ask` to trusted —
  never weaken a stored decision.

## Related documentation

- [`security.md`](security.md) — approval modes, trust boundary
- [`extensions.md`](extensions.md) — extension lifecycle events and
  capabilities
- [`settings-trust.md`](settings-trust.md) — `hooks` settings and trust
  resolution
