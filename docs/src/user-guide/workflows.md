# Isolated concurrent workflows

A **workflow** is a durable, isolated, self-contained run of the agent that
plans an objective into a canonical Todo DAG and then executes that DAG with
delegated subagents. Workflows are managed with `/workflow` and rendered live
in a dedicated TUI page (subagents, tasks, and IRC). The durable lifecycle
lives in `crates/pi-coding/src/workflow/` (manager, store, supervisor) and the
isolation backends in `crates/pi-coding/src/workflow_worktree/` (git worktrees)
and `crates/pi-coding/src/workflow_worktree/overlay.rs` (overlayfs).

## Lifecycle

A workflow moves through a strict status machine
(`WorkflowStatus` in `crates/pi-coding/src/workflow/mod.rs:53-64`):

| Status | Meaning |
|--------|---------|
| `queued` | Created, waiting for a runtime slot. |
| `planning` | A bounded planning turn builds the canonical Todo DAG. |
| `running` | The Todo DAG is being executed by delegated subagents. |
| `paused` | Execution is suspended; `resume` continues it. |
| `integrating` | Committed workflow changes are being merged back. |
| `completed` | All tasks done; integration applied (or nothing to integrate). |
| `failed` | Planning/execution/integration hit a hard error (`WorkflowFailure`). |
| `cancelled` | Explicitly cancelled. |
| `conflicted` | Integration produced merge conflicts. |

Lifecycle transitions are validated in two places: the manager rejects
actions that are not allowed for the current status (`ensure_allowed` in
`workflow/manager.rs`), and the supervisor projects only legal transitions
back (`validate_status`). Pause is legal from `queued`/`planning`/`running`;
resume is legal from `paused`/`planning`/`running`; cancel is legal from any
non-terminal status; integrate is legal from `completed`/`paused`/`conflicted`
(manager) and completes from `completed`/`conflicted`/`failed` (supervisor).

### Transition table with reasons

Every transition below is enforced at `workflow/manager.rs:517-531` (action
gates and runtime projection) and `workflow/supervisor.rs:652-716`
(supervisor-side handling); the reasons are the conditions that make each
transition legal.

| From | Action / event | To | Reason / condition |
|------|----------------|----|--------------------|
| `queued` | `Start` (runtime) | `planning` | The workflow leaves the queue and runs its bounded planning turn. |
| `queued` | `Pause` | `paused` | User `pause` before planning starts. |
| `queued` | `Cancel` | `cancelled` | User cancel; allowed from any non-terminal status. |
| `planning` | `Pause` | `paused` | User `pause` mid-planning; the planning turn is aborted and later `resume` restarts the bounded flow. |
| `planning` | plan committed / budget reached / timed out | `running` | A committed canonical Todo DAG is armed for execution (`arm_plan_and_run` / `preserve_plan_and_run`). |
| `planning` | `Cancel` | `cancelled` | User cancel. |
| `planning` | hard error | `failed` | Planning hit a `WorkflowFailure`. |
| `running` | `Pause` | `paused` | User `pause`; backend parks the worktree and child jobs settle. |
| `running` | `Cancel` | `cancelled` | User cancel; active workflow job ids are cancelled first (`supervisor.rs:702-716`). |
| `running` | DAG settles completed | `completed` | All tasks done or abandoned (`todo_dag_status` = `Settled` + exactly complete, `supervisor.rs:1228-1237`). |
| `running` | DAG settles failed / blocked | `failed` | Settled-but-incomplete, or the DAG is permanently blocked. |
| `paused` | `Resume` | `running` / `completed` | Backend resumes execution; a DAG that finished while paused settles into `completed` and auto-integrates (`manager.rs:438-444`). |
| `paused` | `Integrate` | `integrating` | Manual integration of a paused workflow is allowed. |
| `paused` | `Cancel` | `cancelled` | User cancel. |
| `completed` | `Integrate` (manual or auto) | `integrating` | Auto-integrates whenever the DAG settles into `completed` with no recorded integration (`manager.rs:372-377`); `Conflicted` outcomes land here for manual retry. |
| `integrating` | merge applied | `completed` | Integration outcome `Applied { strategy, result_commit }`. |
| `integrating` | merge conflicts | `conflicted` | Integration outcome `Conflicted { conflicts }` — manual `/workflow integrate` is required. |
| `integrating` | merge error | `failed` | Hard integration error (e.g. `DirtyBase` refused). |

The supervisor may also project `queued → completed`/`failed` (restored
terminal outcomes) and `planning → completed`/`failed`; terminal statuses
(`completed`/`failed`/`cancelled`/`conflicted`) and `paused`/`integrating`
can never be re-entered from a runtime projection — a stale or regressing
projection is rejected (`validate_runtime_projection`).

## `/workflow` command

Canonical usage (`WORKFLOW_USAGE` in `crates/pi-cli/src/workflow_commands/mod.rs:14-15`):

```text
/workflow [list|show [id|name]|create <objective>|create <name> <objective>|pause|resume|cancel|integrate|remove]
```

- Bare `/workflow` opens the dedicated workflows page in the TUI.
- `/workflow list` lists workflows (`status · name · id · objective`).
- `/workflow show [id|name]` shows one workflow's detail.
- `/workflow create <name> <objective>` (or a single argument used as both)
  creates a workflow; the newly created workflow becomes the selection.
- `/workflow pause|resume|cancel|integrate|remove [id|name]` operate on the
  selected workflow, or on the named one. Selectors accept the workflow id or
  an exact name.

Source: `crates/pi-cli/src/workflow_commands/mod.rs` (parse contract and
executor), `crates/pi-cli/src/workflow_rpc.rs` (RPC surface).

The RPC surface (`WorkflowRpcCommand` in `workflow_rpc.rs:87-130`) mirrors
the slash command on the JSONL control plane (see
[`rpc-json.md`](rpc-json.md)):

| `type` | Fields | Notes |
|--------|--------|-------|
| `workflow_create` | `name`, `objective`; optional `id` | Name and objective must be non-empty. |
| `workflow_list` | — | Returns `{ "workflows": [...] }`. |
| `workflow_get` | optional `workflowId` or `name` | One of the two is required. |
| `workflow_pause` / `workflow_resume` / `workflow_cancel` / `workflow_integrate` / `workflow_remove` | `workflowId` | Selector ids resolve exactly; unknown or ambiguous names fail actionably. |

Wire snapshots project the durable `WorkflowSnapshot` (status, integration
`none`/`applied:<commit>`/`conflicted:<paths>`, failure message) and
`workflow_updated` / `workflow_status_changed` / `workflow_removed` events
stream status changes (`workflow_rpc.rs:219-257`).

## Isolation backends

Each workflow runs in its own checkout of the source tree. The backend is
selected by `settings.orchestration.isolation`
(`WorkflowIsolationSetting` in `crates/pi-coding/src/settings.rs:283-298`):

| Setting | Backend | Notes |
|---------|---------|-------|
| `worktree` (default) | git worktree | Branch namespace `rpi/workflow/<id>`, managed root outside the source worktree, ownership catalog at `<managed-root>/pi-workflow/worktrees.json`. Integration fast-forwards or creates a merge commit. Source: `workflow_worktree/git.rs`, `catalog.rs`. |
| `overlayfs` | overlayfs | The source repo is the read-only lower layer; each workflow gets a private writable upper layer. Integration commits the upper state as a single commit on the source branch — last-writer-wins, so `Conflicted` is never produced by this backend. Source: `workflow_worktree/overlay.rs`. |
| `none` | none | No isolation: workflows operate directly on the source working tree (`NoopWorkflowIsolation`). |

The overlayfs backend tries, in order, **kernel overlay** (`mount -t overlay`),
**fuse-overlayfs** (unprivileged FUSE daemon), then **recursive copy** — the
first candidate that succeeds wins, and the chosen backend is persisted per
workflow so a restored workflow re-mounts exactly what it had
(`default_backend_candidates` in `workflow_worktree/overlay.rs:68-75`,
`crates/pi-coding/src/isolate.rs`).

Ownership is strict: every operation verifies that the live checkout exactly
matches the recorded identity (source root, common git dir, branch, HEAD
commit) before touching anything, and removal/prune only ever touch
manager-owned identities (`WorktreeError::OwnershipMismatch`). The managed
root must not be inside the source worktree, and the source base must be
clean before integration (`DirtyBase` is refused).

Integration strategies (`IntegrateStrategy` in
`workflow_worktree/mod.rs:197-206`): `merge` fast-forwards when possible and
otherwise creates a merge commit; `rebase` replays workflow commits onto the
current source HEAD. The outcome is `Applied { strategy, result_commit }` or
`Conflicted { conflicts }` — recorded on the durable snapshot as
`WorkflowIntegration::None | Applied | Conflicted`
(`workflow/manager.rs:23-28`).

## Planning phase

A workflow starts in `queued`, then moves to `planning`: the supervisor agent
(running in the workflow's own `Application` context) receives a bounded
planning prompt (`WorkflowSupervisorContract` in `workflow/supervisor.rs`).
The planning contract requires the model to create the complete canonical
Todo DAG exactly once with the `todo` tool and then stop — no delegation, no
file edits, no waiting on jobs during planning. Explicit agent references in
the objective are validated against the workflow agent catalog before any
planning prompt runs (`validate_delegation_agents`).

Planning is bounded so a correcting model can never hold the workflow in
Planning forever:

- `PLANNING_MAX_TURNS = 8` assistant turns (`workflow/supervisor.rs:35`).
- `PLANNING_MAX_TOOL_CALLS = 16` — cap on Todo/tool calls in one planning
  prompt (`workflow/supervisor.rs:37`).
- `PLANNING_DEFAULT_DEADLINE = 90 s` — one wall-clock budget for the whole
  planning turn, measured from a single pinned sleep (a provider streaming
  keepalives can never stretch it into a deadline-of-silence)
  (`workflow/supervisor.rs:41`, `1051-1061`).
- `PLANNING_IDENTICAL_FAILED_OP_LIMIT = 3` — identical failed Todo
  operations with no Todo-state change terminate planning
  (`workflow/supervisor.rs:44`).
- `PLANNING_CORRECTIONS_WITHOUT_PROGRESS_LIMIT = 6` — semantic
  non-progress detection on the canonical Todo state
  (`workflow/supervisor.rs:46-48`).
- A replan prompt gives a model that answered the first turn with plain text
  one chance to build the plan.

The outcome is one of `Completed`, `PlanCommitted` (plan accepted on the
first successful `todo init`, even while the model still has turns to emit),
`PlanBudgetReached { reason }`, or `TimedOut`. A committed plan (or a budget
stop with tasks present) preserves the plan and transitions to `running`
immediately (`arm_plan_and_run` / `preserve_plan_and_run`).

The supervisor's own planning turn is projected live into the workflow page as
a bounded activity feed (thinking chunks, tool calls, IRC progress) so
planning never reads as a static spinner (`WorkflowSupervisorActivity`,
`workflow/supervisor.rs:85-110`; `planning_started_at_ms` tracks the phase).

## Execution phase

`running` re-arms **Todo DAG execution** over the committed plan: each task
in the canonical Todo state is executed by a delegated subagent (see
[`orchestration.md`](orchestration.md)), with `todoTaskId` linking each job to
its task. The workflow's Todo mutations never auto-arm execution
(`workflow/supervisor.rs:966-984`); arming is explicit on the Planning →
Running transition and on resume of a restored `running` workflow, so a
parked DAG with open tasks is an actively worked workflow, not a Planning
one. Restored workflows never come back frozen: restored `Queued`/`Planning`
resume the planning flow, restored `Running` re-arms execution, restored
`Paused` stays paused until an explicit `resume`
(`WorkflowSupervisor::continue`).

When the DAG settles (all tasks completed or abandoned), the supervisor
projection moves the workflow to `completed` and the manager auto-integrates
when the status ends `completed` with no prior integration
(`workflow/manager.rs:372-377`, `439-444`). Integration is also available
manually with `/workflow integrate`.

## Live detail and workflow panel

The workflow page (`crates/pi-cli/src/workflow_panel.rs`) renders a live,
redacted projection of each workflow (`WorkflowRuntimeDetail` in
`workflow/detail.rs:17-36`):

- `todo` — the canonical Todo DAG.
- `supervisor` — the planning/running supervisor row with its own activity
  feed; during `planning` the (idle at the orchestration layer) supervisor
  reads as actively planning.
- `subagents` — one row per delegated worker: display name, agent type,
  status, current task summary, owning `todoTaskId`, and a bounded per-agent
  activity feed (task lifecycle entries + IRC messages the agent sent or
  received, newest-last).
- `jobs` — delegated orchestration jobs with their todo ownership.
- `irc` — the workflow's recent IRC (subagent ⇄ subagent included), deduped
  by message id and capped at 50 (`RECENT_IRC_LIMIT`).

Everything in the live view is redacted (paths, ids, tokens), and a terminal
durable status always wins over a stale live projection — a `Conflicted`
integration that lands after the supervisor projected `Completed` is shown
as `Conflicted` (`workflow/detail.rs:251-268`). The durable workflow record
never persists activity; the activity feed is live-only.

The supervisor settles from the canonical Todo state and publishes
`WorkflowEvent`s (`StatusChanged`, `Updated`, `Removed`) through the
`WorkflowManager`, which persists durable snapshots to the workflow store
(`workflow/store.rs`) and restores them on restart (`WorkflowManager::reload`).

## Settings

```json
{
  "orchestration": {
    "tasks": false,
    "todo": true,
    "maxConcurrency": 8,
    "maxRecursionDepth": 8,
    "mailboxCapacity": 1000,
    "maxToolsPerAgent": 16,
    "isolation": "worktree",
    "sandboxed": false,
    "softBudget": { "maxRequests": null, "maxTokens": null, "yieldAfter": null }
  }
}
```

- `tasks` — enables the orchestration `task` tool (default `false`).
- `todo` — the `todo` tool is on by default; `false` opts out
  (`settings.rs:1411-1420`).
- `maxConcurrency` — max parallel child jobs (default 8, bounds 1..=64).
- `maxRecursionDepth` — max nested delegation depth (default 8, max 16).
- `mailboxCapacity` — per-agent message mailbox (default 1000, max 10000).
- `maxToolsPerAgent` — tool ceiling per child (default 16, max 64).
- `isolation` — `worktree` (default), `overlayfs`, or `none`; unknown values
  fail deserialization (fail-closed).
- `sandboxed` — when `true`, orchestration subagent children run their
  process spawns (the `bash` tool) inside the Linux filesystem sandbox
  (workspace + agent dir + `sandbox.allowedPaths` visible; deny-by-default
  otherwise). See [`sandbox-isolation.md`](../reference/sandbox-isolation.md).
- `softBudget` — per-child job soft budget: `maxRequests` (LLM turns),
  `maxTokens` (cumulative usage), `yieldAfter` (return control to the parent
  after N requests regardless of remaining budget). A reached limit settles
  the child `Completed` with `softBudgetExhausted: true`, never failed. See
  [`orchestration.md`](orchestration.md).

Validation: `maxConcurrency` must be 1..=64, `maxRecursionDepth` at most 16,
`mailboxCapacity` 1..=10000, `maxToolsPerAgent` 1..=64, and soft-budget knobs
must be positive when set (`settings.rs:2674-2697`).

## Invariants

- Workflows never execute without a committed plan: `running` implies the
  Todo DAG was armed by the supervisor.
- Planning is always bounded: 8 turns, 6 non-progress corrections, and a
  wall-clock deadline; a stuck provider cannot hold a workflow in Planning
  forever.
- Isolation is ownership-checked: every backend operation verifies the exact
  recorded identity before mutating; foreign checkouts fail closed.
- Integration refuses dirty sources and dirty workflows (`DirtyBase` /
  `DirtyWorktree`), and overlayfs integration is last-writer-wins by design.
- The durable snapshot is authoritative for terminal outcomes; the live
  projection may never mask a `Conflicted` or `Failed` state.
- `rewind` is refused while any workflow is active (see
  [`session-recovery.md`](session-recovery.md)).

## Related documentation

- [`orchestration.md`](orchestration.md) — the subagent/job runtime that
  executes the DAG
- [`todos.md`](todos.md) — the canonical Todo DAG the workflow plans and
  executes
- [`sandbox-isolation.md`](../reference/sandbox-isolation.md) — filesystem
  sandbox and overlayfs backends
- [`cli-modes.md`](cli-modes.md) — `/workflow` in the slash-command surface
