# Session recovery: rewind, checkpoints, handoffs, and TTL

Native sessions use Pi v3 JSONL journals. Resume chooses an existing session; fork starts a branch; rewind changes the retained journal history. None of these operations is a Git reset of workspace files.

## Resume and import

`rpi --continue` selects the latest session for the working directory. `--resume PATH_OR_ID` accepts a path, exact identifier, or unambiguous prefix. `--session-dir` changes the storage and lookup root.

```text
rpi import-session pi INPUT
rpi import-session omp INPUT
rpi import-session codex INPUT
rpi import-session claude INPUT
rpi import-session grok INPUT
rpi import-session droid INPUT
```

Imports use the agent-loader library to convert foreign content into native session records. An explicit `--output PATH` must not already exist. Import creates a native session; it does not turn the original foreign file into the live journal.

OMP rotation chains are merged in root-to-leaf order. The leaf supplies identity and working directory; the root anchors the start time. Message records repeated across members are deduplicated by entry identifier, retaining the first occurrence. Members larger than 64 MiB are excluded from conversion but remain lineage boundaries. An import with no usable convertible content fails rather than creating a blank conversation.

When conversion needs a synthetic model hint, it uses `imported`, not a source-tool model name. A retained real model hint is not an assurance that the same model is configured or authenticated in rpi.

## Checkpoint and rewind

`/checkpoint <name>` marks the current journal position. Bare `/rewind` lists the latest 20 record previews. `/rewind <entry-index>` retains records before that index; `/rewind <checkpoint-name>` selects the checkpoint position.

The dropped tail is archived to a rewind JSONL sidecar. Keep sidecars with the session when backing up recoverable history. Rewind does not undo commands, remote effects, or file writes already performed by tools.

## Compaction and handoff

`/compact` summarizes context through the model. `/compact --snap` and `/snapcompact` archive dense history deterministically without a model call. Snap archives preserve history separately from the compact active context.

`/handoff` produces a deterministic handoff envelope without a provider call. `/handoff --prose` requests prose generation. Other flags are rejected. This interactive summary operation must not be confused with the Web session-handoff command, which creates a new session and switches the view through the subscription handoff protocol.

## Session lifetime

Native session pruning defaults to **30 days**, controlled by `sessionTtlDays`. Expiry is based on last modification, not creation time. The pruning code skips recently modified files through an active-session grace window and accepts explicit file/directory exclusions. Foreign session roots are not pruned as native sessions.

Export important sessions and their recovery sidecars before relying on retention. Disabling persistence with `--no-session` does not provide a recoverable journal.

## In-flight queue state

Steering queue integration is still in progress on the Rust side. Queued previews in the Web client do not establish a durable queue-recovery contract. Inspect restored session state before resubmitting work.
