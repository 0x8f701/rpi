# Orchestration: subagents, jobs, and IRC

Orchestration runs child coding sessions and delivers their results to the parent. It is separate from operating-system process supervision and from recurring prompt scheduling.

## Child tasks

The model-facing `task` tool accepts either one `task` string or a nonempty `tasks` array, never both. Each child briefing must contain its objective, concrete work, and acceptance criteria. A batch is limited to 64 items.

Optional task fields include `name`, `agent`, `context`, `todoTaskId`, `outputSchema`, and `schemaMode`. A batch item has its own task text and optional configuration. Shared context precedes item-specific context. The enabled agent definitions determine the selectable agent types; the configured recursion depth limits further child spawning.

Task creation returns child/job identities. Use those returned identities instead of guessing a peer name. A child's status and its delivered result are different things: starting a job does not deliver its answer.

## Communication

The model-facing `hub` tool supports peer messaging and child-job coordination: `send`, `ask`, `wait`, `inbox`, `list`, `read_history`, cancellation, and status operations. Request/reply communication uses correlation identifiers. IRC messages appear as structured transcript content with sender, recipient, body, and reply context.

`hub` does **not** supervise operating-system processes. Use the process tool or Processes panel for a server, watcher, shell process, or interactive program.

## Result delivery

An orchestration child receives a `yield` tool. Its request contains a required `text` string with the final deliverable. Calling it records the delivered payload and ends the child's run at the turn boundary. The main-session model does not receive this tool. Repeated delivery is ignored rather than replacing the result.

Structured output requirements belong to the task contract. A parent should inspect the returned result and verify its requested acceptance criteria; a completed job status alone is not evidence that its edits or conclusions are correct.

## Supervised processes

Processes have their own identifiers, bounded output, state, and stop lifecycle. The Web [Processes panel](web.md#processes) runs commands through `bash -c` in the session working directory, exposes per-process output, and polls every three seconds while mounted. Stop sends termination, allows a grace period, and escalates to forced kill when required.

The model-facing bash tool permits `grep` and `rg`. Unsupervised background or detached shell work is still rejected. Long-lived work must use `background=true` or the process tool so the runtime can track and stop it.

## Recurring prompts

`/loop 5m Check the current build status` schedules a recurring prompt through the application runtime, not a shell or nested CLI. Positive bare seconds and `s`, `m`, `h`, or `d` suffixes are accepted. `/loop list`, `/loop delete <id>`, `/loop cancel <id>`, and `/loop update <id> [interval] [prompt]` manage tasks and execution.

The Web Loop panel creates non-durable loops that fire immediately, then repeat at the chosen interval. Its presets are 30s, 1m, 5m, 15m, 1h, and 4h. Loop and Processes are separate panels.

## Session ownership and safety

Child runtime state, transcript routing, and durable orchestration bindings are associated with the owning session. Do not use another session's identifiers as interchangeable handles, or assume changing the displayed session cancels background work.

Delegating execution does not remove approval or sandbox requirements. Give children explicit file ownership and complete acceptance criteria, and review their output before accepting it. See [isolated workflows](workflows.md) when concurrent work needs worktree isolation.

**Steering queue status: in progress.** Web queued-prompt previews are implemented; Rust queue cancellation and its scope/index integration remain in flight. This page does not promise durable queue behavior or a stable scoped-cancellation interface.
