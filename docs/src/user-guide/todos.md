# Todos and the Todo DAG

Todos are the task-level plan of a session: an ordered set of **phases**, each
holding **tasks** that can depend on other tasks, forming a directed acyclic
graph (DAG). The `todo` tool (on by default; disable with
`settings.orchestration.todo = false`) lets the model create, edit, and
execute the plan in natural language, and the TUI renders it in a dedicated
Todo DAG panel. Workflows plan into the same canonical structure and execute
it with delegated subagents (see [`workflows.md`](workflows.md)).

## Model

- `TodoStatus` — `pending`, `in_progress`, `completed`, `abandoned`
  (`crates/pi-coding/src/todo.rs`).
- `TodoItem` — `id` (stable `task-<uuid>`), `content`, `status`,
  `depends_on` (dependency task ids), `ready`, `blocked_by` (derived
  blocked-reasons with the blocking task's content and status), and an
  optional `agent` (a typed routing contract naming the agent that must
  execute the task).
- `TodoPhase` — `name` + `tasks`.
- `TodoState` — `phases` + `storage` (`Session` — persisted in the session
  file — or `Memory`).

Readiness is projected after every mutation: a `pending`/`in_progress` task
is `ready` only when all of its `depends_on` tasks are `completed` or
`abandoned`; otherwise it is `blocked_by` each unfinished dependency
(`project_readiness` in `todo.rs`). Cycle detection rejects any mutation that
would make the dependency graph cyclic (`graph_contains_cycle`), and phase
names/task contents are normalized and validated before an operation commits
(`prepare_todo_phases`).

## `todo` tool operations

The tool is a single `todo` operation with these ops (`TodoOp` in `todo.rs`,
serialized `snake_case`):

| Op | Fields | Effect |
|----|--------|--------|
| `init` | `list` (phases with `items` and optional parallel `agents`), or `items`/`phase` | Create the plan. A re-init describing exactly the current plan preserves ids/dependencies/statuses; otherwise it replaces the plan. |
| `append` | `phase`, `items` | Add tasks to a phase. |
| `start` | `task` | Mark a task `in_progress`. |
| `done` | `task` or `phase` | Mark task(s) `completed`. |
| `drop` | `task` or `phase` | Mark task(s) `abandoned`. |
| `rm` | `task` or `phase`, `cascade` | Remove tasks/phase; `cascade` removes dependents too. |
| `add_dependency` | `task`, `dependsOn` | Add dependency edges (cycle-checked). |
| `remove_dependency` | `task`, `dependsOn` | Remove dependency edges. |
| `update_dependencies` | `task`, `dependsOn` | Replace the dependency set. |
| `view` | — | Render the current plan. |

An `init` phase may carry `agents[i]` names parallel to `items[i]`: that task
is routed to the named agent during DAG execution (validated against the
agent catalog — unknown or disabled agent names fail actionably).

Results report the mutated phases, the list of tasks that just transitioned
to `completed` (`completedTasks`), and a plain-text summary. The todo tool is
transactional under orchestration: mutations run inside a gate with a
check/commit pair so DAG execution and planning cannot race
(`TodoMutationTransaction`).

## `/todo` and the Todo DAG panel

In the TUI and REPL:

```text
/todo            # open the Todo DAG panel (TUI) / print the plan (REPL)
/todo list       # print the plan
/todo markdown   # print the plan as Markdown checklists
```

Source: `crates/pi-cli/src/interactive_commands.rs` (`/todo` builtin),
`crates/pi-cli/src/todo_dag_panel.rs` and `crates/pi-cli/src/todo_dag_view.rs`
(panel), `crates/pi-cli/src/todo_dag_panel.rs` (`TodoDagPanel`).

The Todo DAG panel (`TodoDagPanel` in `todo_dag_panel.rs`) shows every DAG in
the session: the main session's DAG plus one DAG per active workflow. Each DAG
header renders the execution label and counts (`✓ done · open · active ·
blocked`); subagent rows under it show `• <name> (<agent>) · <status> ·
<current task>` with the owning `todoTaskId`. Keys: `↑/↓`/`j`/`k` select a DAG
or subagent row, `Enter` opens the detail page (task list with `depends_on`
and `blocked_by`, plus linked jobs per task) or the subagent page (identity,
type, status, owning DAG, linked todo task, current task summary, progress).
`Esc` returns to the overview, `Esc`/`q` closes.

## DAG execution

The DAG executes in two places:

- **Orchestration**: `task` calls pass a `todoTaskId` when the child owns a
  canonical Todo DAG item (`orchestration/tools.rs`); the runtime records the
  ownership on the job snapshot, and the panel deduplicates subagent rows
  against the DAG by task identity.
- **Workflows**: the supervisor arms execution over the committed plan
  (`workflow/supervisor.rs::arm_plan_and_run`); a workflow's Todo mutations
  never auto-arm — arming happens on Planning → Running and on resume. See
  [`workflows.md`](workflows.md).

## Steering and follow-up queues (`/queue`)

The session maintains two prompt queues with independent modes
(`QueueMode`: `all` or `one-at-a-time`), configured by
`settings.steeringMode` and `settings.followUpMode` (default `all`):

- **steer** — an interrupting prompt delivered into the active turn.
- **follow-up** — a prompt queued for the next turn.

When a mode is `one-at-a-time`, only one queued message of that kind is
delivered at a time; the rest stay pending.

```text
/queue            # show pending steering/follow-up prompts
/queue cancel     # clear both queues
```

Source: `crates/pi-cli/src/interactive_commands.rs` (`/queue` builtin),
`crates/pi-coding/src/application.rs` (`queued_messages`/`drain_queued_messages`),
`crates/pi-coding/src/session.rs` (queue storage). The RPC surface exposes
`set_steering_mode`/`set_follow_up_mode` and `prompt` with
`streamingBehavior` `"steer"`/`"followUp"` (see [`rpc-json.md`](rpc-json.md)).

## RPC

`set_todos` replaces the session's phases (`{"type":"set_todos","phases":[...]}`);
`todo_updated` events announce changes. See [`rpc-json.md`](rpc-json.md).

## Invariants

- The Todo DAG is always acyclic; cyclic mutations are rejected.
- Readiness is derived, never stored: `ready`/`blocked_by` are recomputed
  from `depends_on` + statuses after every mutation.
- `completed` transitions are reported exactly once per task per mutation
  (`completion_transitions` compares before/after).
- A re-init that matches the current plan is a no-op on ids/dependencies —
  stable `todoTaskId`s survive re-planning of an identical plan.
- The `todo` tool is available to orchestration children by default and is
  never removed by role ceilings (orchestration plumbing).

## Related documentation

- [`workflows.md`](workflows.md) — workflow planning/execution over the DAG
- [`orchestration.md`](orchestration.md) — `todoTaskId` ownership and jobs
- [`goals.md`](goals.md) — the goal (why), distinct from todos (what)
- [`rpc-json.md`](rpc-json.md) — `set_todos`, `todo_updated`, steering
