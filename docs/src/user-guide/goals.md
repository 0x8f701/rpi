# Goals

A **goal** is a durable, session-scoped objective that describes why future
turns may continue. It is deliberately separate from todos, plans, scheduled
loops, and conversation messages: goals live in their own state machine with a
lifecycle, an optional token budget, role-model pins, and a revisioned journal
(`crates/pi-coding/src/goal.rs`).

The goal module only records state and returns a continuation decision — it
never starts a turn. The Application layer decides whether to keep an active
goal's work running and whether a goal turn should be queued or fired
(`crates/pi-coding/src/application.rs::activate_goal`).

## Lifecycle

A goal is one of (`GoalLifecycle`, `goal.rs`):

| Lifecycle | Meaning |
|-----------|---------|
| `active` | The goal is current and its work may continue. |
| `paused` | The goal exists but its work is suspended. |
| `completed` | The objective is done. |
| `dropped` | The objective is abandoned. |

Only one goal is current at a time (`GoalState.current`). The state carries a
`revision` counter; every transition is appended to the session journal as a
typed event (`GoalEvent`), and the journal is replayed to reconstruct state on
resume (`goal_events_from_session_tree`). Malformed or future goal entries
fail closed during replay.

## Usage

```text
/goal show                       # inspect the current goal
/goal create <objective>         # set a goal (bare text is a create shorthand)
/goal create --tokens N <obj>    # set a goal with an explicit token budget
/goal pause                      # pause goal work
/goal resume                     # resume goal work
/goal complete                   # mark the objective done
/goal drop                       # abandon the objective
/goal pin <text>                 # add a role-model pin
/goal pins                       # list pins
/goal unpin <index>              # remove pin at 0-based index
```

Source: `crates/pi-cli/src/goal_commands.rs` (parse contract and executor),
`crates/pi-coding/src/application.rs` (goal activation, pause, resume,
complete, drop, pin, unpin).

## Budget and usage

A goal may carry a `tokenBudget` (set with `--tokens N`; zero is rejected).
Usage is tracked as `GoalUsage { tokens_used, active_time_seconds }`: the
runtime accumulates model usage and active wall-clock time while the goal is
active, and the goal continuation decision accounts for the budget. Without a
budget the goal runs without a token ceiling. The `/goal show` summary renders
`active · 123/500 tokens · <objective>` and includes time spent and pins
(`goal_commands.rs::format_goal_details`).

## Pins

A goal supports up to `MAX_GOAL_PINS = 8` **pins** — short example or
instruction strings (at most `MAX_GOAL_PIN_CHARS = 200` characters each) shown
verbatim in the goal turn's context as role models. Pins are managed with
`/goal pin <text>`, `/goal pins`, and `/goal unpin <index>`.

## Journal and invariants

Every goal event records both the transition and its resulting snapshot
(`GoalEventKind` + `goal`), revisioned and validated against the previous
state before it is appended (`validate_replayed_transition`). The typed
event kinds (`GoalEventKind` in `goal.rs:120-133`):

| Event kind | Payload | Meaning |
|------------|---------|---------|
| `created` | — | A goal was created (must be the journal's first event). |
| `fork_cloned` | `source` | A forked session cloned this goal from a source snapshot. |
| `paused` | `reason` (`manual` / `resume_safety` / `budget_exhausted`) | Goal work was suspended; `resume_safety` is the forced pause on session resume, `budget_exhausted` can only be resumed by a fresh budget. |
| `resumed` | — | Goal work resumed from `paused`. |
| `completed` | — | Objective marked done. |
| `dropped` | — | Objective abandoned. |
| `usage_updated` | `delta` | Token/time usage accumulated while active. |
| `pins_updated` | `pins` | A pin was appended or removed; carries the resulting pin list. |

Replay validation (`validate_replayed_transition`, `goal.rs:880-993`) rejects
any event that follows a terminal lifecycle (except `usage_updated`), a
`resumed` event after `budget_exhausted` pausing, `completed`/`dropped` with a
pause reason or changed pins, and `usage_updated` deltas inconsistent with the
previous snapshot. A `created` event after the first one, a `fork_cloned`
whose `source` does not match the current goal, and malformed or future-version
entries all fail closed.

- The journal is stored in the session file as a custom entry type
  `pi.goal.event` (`GOAL_SESSION_CUSTOM_TYPE`) at version
  `GOAL_SESSION_ENTRY_VERSION = 1`.
- Objective size is capped at `MAX_GOAL_OBJECTIVE_BYTES = 64 KiB`, well under
  the session record limit.
- A fork clones the current goal state; the cloned goal is validated so a
  forked journal cannot diverge (`validate_fork_cloned`).
- Goal usage is session-scoped: it accumulates across turns while active and
  is serialized with the goal, so a resumed session continues the same
  budget accounting.
- `rewind` (see [`session-recovery.md`](session-recovery.md)) treats goal
  journal entries as regular session entries; rewinding past a goal event
  rolls the goal back to the corresponding prior snapshot.

## The `goal` tool

Orchestration children receive a `goal` tool exposing the same operations
(create/show/pause/resume/complete/drop) so a delegated worker can manage its
own durable goal. See [`orchestration.md`](orchestration.md).

## Related documentation

- [`todos.md`](todos.md) — the task-level plan, distinct from the goal
- [`orchestration.md`](orchestration.md) — the `goal` tool in child sessions
- [`session-recovery.md`](session-recovery.md) — the goal appears in handoff
  envelopes (`HandoffGoal` in `crates/pi-coding/src/handoff.rs`)
