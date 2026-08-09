# Session recovery: rewind, checkpoints, handoffs, and TTL

This page covers the session-level recovery and hygiene features: rewinding to
an earlier point in the journal, named checkpoints, deterministic snap
compaction, handoff summaries, doom-loop recovery, and startup TTL pruning of
old session files.

## `/rewind` and `/checkpoint`

```text
/rewind                       # list entry indices and checkpoints to pick from
/rewind <entry-index>         # roll the session back to before that entry
/rewind <checkpoint-name>     # roll back to the named checkpoint
/checkpoint <name>            # mark the current position as a named rewind target
```

`/rewind` truncates the session journal back to a target and **archives the
dropped tail** to a sidecar (the session store keeps every file; nothing is
lost). The bare command lists the last 20 records with first-line previews,
annotating checkpoints with the entry they target. `/checkpoint <name>` marks
the current position so `/rewind <name>` can return to it later; a checkpoint
named like a number is unreachable by design (numbers always parse as entry
indices).

Sources: `crates/pi-cli/src/interactive_commands.rs`
(`parse_rewind_invocation`, `format_rewind_list`, `format_rewind_outcome`),
`crates/pi-coding/src/application.rs` (`rewind`, `set_checkpoint`,
`rewind_preview`).

**Safety:** rewinding is refused while any orchestration job is queued or
running, while any workflow is active, or while `bash` is still executing —
truncating the journal under live work would orphan running jobs and corrupt
the state those jobs keep reading. The active turn is drained first so the
checks observe post-turn state, and the refusal message names the count of
live jobs and suggests `orchestration /queue cancel` or
`workflow /workflow cancel` (`rewind_refusal` in `application.rs:2250-2264`).

## `/snapcompact` and `/compact --snap`

```text
/snapcompact                  # deterministic archive, no LLM call
/compact --snap               # same (trailing text after --snap is ignored)
/compact [instructions]       # legacy LLM summarization path
```

`compact_snap` (in `application.rs:2077-2080`) replaces older turns with a
**deterministic statistics block** — per message-type counts, total archived
characters, timestamp span, sorted unique tool names, and a bounded list of
user-ask first lines (`build_snapcompact_summary` in
`crates/pi-coding/src/compaction.rs:471`) — and preserves the original
entries in a `.snapcompact-<timestamp>.jsonl` sidecar. The cut point never
splits a turn (`find_snap_cut_point`). Useless tool results (empty or
duplicate error text) are elided with a note.

The LLM path uses the structured checkpoint summary prompt
(`SUMMARIZATION_PROMPT` in `compaction.rs:64-65`) and rebuilds the view as a
`CompactionSummary` message followed by the recent entries
(`apply_checkpoint`). Automatic compaction triggers on context threshold
(`should_compact` with `compaction.enabled`/`reserveTokens`/
`keepRecentTokens`); see [`settings-trust.md`](../reference/settings-trust.md).

## `/handoff`

```text
/handoff                      # deterministic envelope (copied to clipboard)
/handoff --prose              # envelope + one bounded prose paragraph
```

A handoff is a concise, structured summary of the current session — what was
done (recent user asks), current state (active goal, todo counts, running
orchestration jobs), environment (cwd, git branch and dirtiness, model), and
deterministic next-step hints. The envelope is built entirely from session
state with **no model call** (`handoff_envelope` in
`crates/pi-coding/src/handoff.rs:226`); `--prose` adds one paragraph from the
existing summarization path, bounded to a single provider call with a hard
60-second timeout and a 600-token reserve (`HANDOFF_SUMMARIZE_TIMEOUT`,
`HANDOFF_PROSE_RESERVE_TOKENS`).

The rendered block (`# Handoff`) is copyable plain text, safe to paste into a
fresh session. Everything in it is redacted for credential-shaped text, and
only queued/running jobs appear (`active_handoff_jobs`). Hints are capped at
4 (`HANDOFF_MAX_NEXT_STEP_HINTS`).

Source: `crates/pi-cli/src/interactive_commands.rs` (`/handoff` builtin),
`crates/pi-coding/src/handoff.rs`.

## Doom-loop recovery

The session turn loop detects a **doom loop** — the same tool failing with
the same error prefix `DOOM_LOOP_THRESHOLD = 3` consecutive times in one
turn — and stops the turn with an actionable message instead of letting the
model retry the same failing call forever (`DoomLoopTracker` in
`crates/pi-coding/src/session.rs:189-232`, `doom_loop_recovery` at
`session.rs:4553`). Error prefixes are fingerprinted at
`DOOM_LOOP_ERROR_PREFIX_CHARS = 80` characters; transient markers ("timed
out", network blips) never count toward the threshold. Once tripped, every
further tool outcome in the turn terminates with the same message so a
parallel batch cannot escape the stop, and the turn errors out with it.

## Session TTL cleanup

At startup, `rpi` prunes old native session files:

- Default age: `DEFAULT_SESSION_TTL_DAYS = 30`; override with
  `settings.sessionTtlDays` (must be > 0).
- Files modified within `SESSION_ACTIVE_GRACE = 1 hour` are treated as
  possibly active and **never** pruned (the native store has no lock files).
- Pruning touches only the native pi session tree (files at up to two
  directory levels below a root); foreign sources (codex/claude/grok) live in
  separate roots and are never walked.
- Never pruned: the current session's file, the live run's directory,
  symlinks (never followed or deleted), and files with future/implausible
  mtimes (`PRUNE_MTIME_FLOOR_SECS`).

Source: `crates/pi-coding/src/session_store.rs` (`prune_expired_sessions`,
`session_file_expired`). The recorder self-heals if a prune removed its (as
yet empty) per-cwd directory before the first flush.

## `/fresh`

`/fresh` (alias for `/new`) archives the current session in place and starts
a clean session — the session store keeps every file, so the old recorder
stays on disk. See [`export-share.md`](../reference/export-share.md).

## Invariants

- Rewind never loses data: the dropped tail is archived, and rewinding is
  refused while any live work (jobs, workflows, bash) could be orphaned.
- Snapcompact never calls the provider and never splits a turn; the original
  entries survive in a sidecar.
- Handoff envelopes are deterministic and redacted; prose is a single bounded
  provider call.
- Doom-loop recovery is per-turn and transparent (transient errors never
  count), and it cannot be escaped by a parallel tool batch.
- TTL pruning is conservative: grace-window, current-session, foreign,
  symlinked, and clock-skewed files are never deleted.

## Related documentation

- [`export-share.md`](../reference/export-share.md) — `/export`, `/dump`,
  `/share --encrypt`, `/fresh`
- [`settings-trust.md`](../reference/settings-trust.md) — `compaction`,
  `sessionTtlDays` settings
- [`goals.md`](goals.md) — goal journal entries participate in rewind
