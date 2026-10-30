# Memory

Memory stores reusable project notes independently of the active conversation context. The configured backend is `local` by default, `hindsight` for the external service, or `off` to hide memory tools.

## Local backend

The `memory` tool supports:

| Action | Effect |
| --- | --- |
| `learn` | Append content with tags, write time, and source-session metadata |
| `recall` | Case-insensitive content/tag search, ranked by match strength then recency |
| `list` | Newest-first listing with optional exact tag filter |
| `forget` | Remove the entry identified by a returned identifier |

The store is `<agent-dir>/memory/<repo-digest>/entries.jsonl`. The repository namespace is derived from the working directory, anchored at a Git ancestor when present. It deliberately persists across sessions in the same checkout; it is not a private transcript namespace for each session.

The local store retains at most **100 entries**, evicting the oldest on learn. Entry content is capped at **1 MiB**, tool output at **32 KiB**, and recall returns **10** entries by default with a maximum of **50**. Writes are atomic and serialized within the process. Separate processes sharing the same store do not have cross-process writer coordination.

A repository move migrates notes additively and idempotently into the new namespace; the old archive remains recovery metadata until move finalization.

## External backend

With `memory.backend: "hindsight"`, the tools are `recall`, `retain`, and `reflect`, backed by the configured HTTP service. Operations have explicit timeouts and bounded requests/responses. Plaintext endpoints require an explicit insecure opt-in. Service credentials are not rendered into tool output.

`off` disables all memory tools; it does not mean existing stored notes were deleted.

## Safety

Local notes are ordinary model-authored text, not a credential vault. The persistence boundary redacts obvious secret patterns, but that is best-effort protection. Never store real credentials or assume every sensitive string will match a redactor.

Recall is advisory context. Verify a recalled note against current files and runtime state before relying on it. Local learning is model-invoked; do not assume automatic extraction from every conversation turn.
