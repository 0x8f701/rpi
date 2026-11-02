# Hooks and trust hooks

Host hooks are external commands configured in `hooks`. They execute an argument vector directly, not through an implicit shell. A matching event sends a JSON payload on standard input; bounded standard output is parsed as JSON.

## Decisions

Only `pre_tool_call` and `pre_trust_decision` honor a blocking response:

```json
{"decision":"block","reason":"The requested operation is not permitted"}
```

Other hook events are advisory. A spawn failure, nonzero exit, timeout, or malformed JSON fails open for the two blocking events unless the hook explicitly configures `fail_closed: true`. With fail-closed behavior, such a failure blocks the tool or denies the tentative trust decision.

This distinction matters: writing a policy hook without fail-closed configuration does not guarantee denial when the hook itself breaks.

## Payload and bounds

Payloads identify the event, subject, working directory, session, timestamp, and bounded tool argument/result summaries where applicable. Secrets are not intended to be included. Treat every supplied string as untrusted input and avoid echoing raw payloads into logs.

The default timeout is **5 seconds**, the maximum timeout is **60 seconds**, and captured standard output is capped at **64 KiB**. Output beyond the cap is drained and discarded. Argument and result summaries have their own smaller bounds.

## Trust ordering

Host `pre_trust_decision` hooks run at the resource admission boundary before project extensions are loaded. Project code cannot authorize its own loading. A denial must not be upgraded through an extension path.

Host hooks and extension event hooks are different mechanisms. The latter require admitted extension capabilities; see [extensions](extensions.md).

## Security

Hooks execute code under host or governed candidate/published authority according to their lifecycle stage. Install only hooks you trust, use explicit executable arguments, and configure failure policy deliberately. A hook is not a replacement for filesystem or network confinement.
