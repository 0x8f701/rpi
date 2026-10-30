# Todos and the Todo DAG

A Todo list groups tasks into named phases. A task has an identifier, content, status, dependency identifiers, derived readiness/waiting information, and an optional explicit agent. A directed acyclic graph (DAG) expresses which tasks must finish before another can run.

## Status and dependencies

Task statuses are `pending`, `in_progress`, `completed`, and `abandoned`. `ready` and `waitingOn` are projections of the dependency state, not independent status controls. An explicit `agent` is validated against the orchestration catalog when execution begins.

Do not confuse task status with coordinator status. The parent-owned Todo executor reports `dormant`, `active`, `settled`, or `failed`. Its failure reasons distinguish no progress, spawning, and reconciliation failures.

## Interactive operations

The `/todo` command provides list management and child-run controls:

```text
/todo list
/todo new Parser review
/todo select <id>
/todo status [id]
/todo send [id] <message>
/todo add <id> [options] <task>
/todo pause [id]
/todo resume [id]
/todo stop [id]
/todo cancel [id]
/todo archive <id>
/todo start <task>
/todo done <task>
/todo drop <task>
/todo clear
```

Angle-bracket values are identifiers or text supplied by the operator; optional identifiers target the selected run where the command supports selection. Use the returned identifiers and the current list rather than guessing them. `/todo` also accepts Markdown task input.

The Todo panel exposes list state and execution detail. Child coding jobs and their communication use the [orchestration](orchestration.md) runtime; a Todo item is not an operating-system process.

## Persistence and restart

A session-backed Todo snapshot stores phases and executor state in the session journal. On restore, a previously `active` executor becomes `failed` with an interrupted-by-process-restart reason. Running work is **not** automatically replayed. Inspect open tasks, then explicitly restart execution.

Storage supports session-backed and memory-backed state. Do not assume an in-memory list survives process exit.

## Task contract

Keep each task's outcome and dependencies explicit. A completed child job is not automatically proof that its requested result is correct. Review output and evidence before completing the corresponding task. Do not use dependency metadata to hide failed work or fabricate readiness.

See [Goals](goals.md) for a session objective and budget, and [workflows](workflows.md) for worktree-isolated execution.
