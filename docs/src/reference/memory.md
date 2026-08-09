# Memory

`rpi` has a memory subsystem with three backends, selected by
`settings.memory.backend` (`crates/pi-coding/src/memory.rs`):

| Backend | Tools | Behavior |
|---------|-------|----------|
| `local` (default) | `memory` | Built-in JSONL note store: `learn`, `recall`, `list`, `forget` entries that survive across sessions. Ordinary sessions use a repository namespace; personas use their durable persona-local store. |
| `hindsight` | `recall`, `retain`, `reflect` | Calls an explicitly configured Hindsight HTTP API. |
| `off` | (none) | Every memory tool is hidden. |

A missing config falls back to `local`. Backend selection is inherited by future
ordinary and persona child sessions; local persona memory remains rooted at the
persona directory.

## Settings

```json
{
  "memory": {
    "backend": "hindsight",
    "hindsightApiUrl": "https://memory.example.test",
    "hindsightApiToken": "$HINDSIGHT_API_TOKEN",
    "hindsightBankId": "rpi",
    "hindsightScoping": "per-project-tagged",
    "hindsightInjection": false
  }
}
```

| Key | Default | Meaning |
|-----|---------|---------|
| `memory.backend` | `local` | `local`, `hindsight`, or `off`. |
| `memory.hindsightApiUrl` | unset | Explicit Hindsight HTTP API base URL. Required for `hindsight`. |
| `memory.hindsightApiToken` | unset | Optional bearer token. Secret settings views always redact it. |
| `memory.hindsightAllowInsecure` | `false` | Explicitly permit plaintext HTTP endpoints and redirect hops for a trusted self-hosted service. Secure mode is HTTPS-only for the initial URL and every redirect. |
| `memory.hindsightBankId` | `rpi` | Base bank id. |
| `memory.hindsightBankIdPrefix` | unset | Optional bank-id prefix. |
| `memory.hindsightScoping` | `per-project-tagged` | `global`, `per-project`, or `per-project-tagged`. |
| `memory.hindsightBankMission` | unset | Optional reflect mission applied while ensuring the bank exists. |
| `memory.hindsightRetainMission` | unset | Optional retain mission applied while ensuring the bank exists. |
| `memory.hindsightInjection` | `false` | Inject bounded recall for the latest ask as hidden context. |
| `memory.hindsightRecallBudget` | `mid` | Hindsight recall/reflect budget: `low`, `mid`, or `high`. |
| `memory.hindsightRecallMaxTokens` | `1024` | Maximum tokens requested from recall. |
| `memory.hindsightRecallTypes` | `world`, `experience` | Memory types included in recall. |
| `memory.hindsightRequestTimeoutMs` | `30000` | Default HTTP request deadline. |
| `memory.hindsightRecallTimeoutMs` | `30000` | Recall deadline. |
| `memory.hindsightRetainTimeoutMs` | `60000` | Retain deadline. |
| `memory.hindsightReflectTimeoutMs` | `120000` | Reflect deadline. |

## Local backend

Ordinary sessions store entries at:

```text
<agent-dir>/memory/<repo-digest>/entries.jsonl
```

Persona sessions store entries at:

```text
<persona-root>/memory/entries.jsonl
```

The public local behavior remains unchanged:

- Entries are capped at 1 MiB.
- At most 100 entries are retained per namespace; `learn` evicts the oldest.
- `recall` defaults to 10 entries and clamps at 50.
- At most 20 tags are stored per entry.
- Output is bounded and secrets are redacted.
- Switching away from local never migrates, mirrors, deletes, or rewrites JSONL.

## Hindsight backend

Hindsight uses the source-verified HTTP wire contract:

- `POST /v1/default/banks/{bank}/memories/recall`
- `POST /v1/default/banks/{bank}/memories`
- `POST /v1/default/banks/{bank}/reflect`
- `PUT /v1/default/banks/{bank}` before retain/reflect to idempotently create
  or update the bank and apply optional missions. Any non-success, timeout, or
  authentication failure stops retain/reflect instead of being ignored.

Recall and reflect requests include the selected budget, recall types, token
limit, and namespace tags; retain requests include the item content and tags.
The client methods themselves reject empty required query/content fields and
bound every query, content, and optional context to 1 MiB, including direct
turn-start injection calls that bypass tool argument validation.

Responses are capped at 256 KiB before JSON decoding; rendered tool and
injection output remains capped at 32 KiB. Every operation has an explicit
timeout. HTTP errors include the operation and status while redacting credential
shapes in the error path. The bearer token is sent only in the Authorization
header. `MemoryConfig` debug output renders a redaction marker instead of the
token.

Plaintext HTTP endpoints are rejected unless `hindsightAllowInsecure` is
explicitly true. In secure mode, the HTTP client is HTTPS-only and a redirect
policy checks each hop before following it: HTTPS-to-HTTP and every other
plaintext redirect fail before a request reaches the plaintext target, so the
Authorization header cannot be forwarded there. The explicit insecure opt-in
permits both an HTTP base URL and HTTP redirect hops for trusted self-hosted
deployments; the normal redirect limit still applies.

Turn-start injection is advisory and fail-open: an unavailable Hindsight service
never prevents an ordinary agent turn from running. Explicit tool calls return
contextual errors and never silently fall back to local.

## Namespaces

- `global`: one bank, no project tag filter.
- `per-project`: bank id is `<base>-<project-label>`.
- `per-project-tagged`: one bank; retains carry `project:<label>` and recalls
  use that tag with `tags_match: any`, which also permits untagged global memory.
- The project label derives from the canonical repository anchor when present,
  otherwise the working-directory name.
- Future child sessions resolve the parent's live backend configuration. Local
  persona children still use `<persona-root>/memory/entries.jsonl`; Hindsight
  persona children use the explicitly configured bank/scoping contract.

## Invariants

- Exactly one backend is active; no dual writes or automatic fallback.
- `off` removes the complete reserved memory tool family in parent and child
  sessions.
- Local JSONL paths and model-visible local tool behavior are unchanged.
- External endpoints, auth, HTTPS-only redirect enforcement, timeouts, request/
  response bounds, and plaintext opt-in are explicit.
- Turn-start context is hidden (`display: false`) and never auto-submitted.

## Related documentation

- [`tools.md`](tools.md) — the full tool catalog including memory
- [`settings-trust.md`](settings-trust.md) — settings precedence and trust
