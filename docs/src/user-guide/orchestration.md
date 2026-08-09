# Orchestration: subagents, jobs, and IRC

The orchestration runtime (`crates/pi-coding/src/orchestration/`) lets the
main session delegate work to child coding sessions ("subagents"), supervise
them as **jobs**, and coordinate between them with **IRC-style mailbox
messages**. It powers the `task`, `hub`, `yield`, and `goal` tools, the
workflow executor (see [`workflows.md`](workflows.md)), and the TUI's agents
panel and Todo DAG view.

## Enabling

Orchestration is opt-in. Set `settings.orchestration.tasks = true` (the
`task` tool gate; `orchestration_enabled` in
`crates/pi-coding/src/settings.rs:1403-1409`). The `todo` tool is on by
default and the `process` tool is gated by `orchestration.process`.

```json
{
  "orchestration": { "tasks": true }
}
```

The workflow runtime requires orchestration and gates it separately; see
[`workflows.md`](workflows.md) for the full `orchestration` settings block.

## Agent catalog

Agents are Markdown definitions with YAML frontmatter, discovered from
`<agent-dir>/agents/*.md` and (when the project is trusted)
`<workspace>/.pi/agents/*.md` (`AgentCatalog::discover` in
`orchestration/definitions.rs:138`). The bundled `task` and `researcher`
agents are always available; user definitions win over bundled ones.

Frontmatter fields (`AgentDefinition` in `orchestration/definitions.rs:66-88`):

| Field | Meaning |
|-------|---------|
| `name` | Identifier used by `task`/`hub` and by delegation. |
| `description` | Shown in the `task` tool's available-agent list. |
| `tools` | Child tool allowlist; `settings.agents.<name>.tools` overrides it. |
| `autoloadSkills` | Skills autoloaded into the child's prompt. |
| `model` | Model pattern list; `settings.agents.<name>.model` overrides it. |
| `thinkingLevel` | Child reasoning level. |
| `maxTurns` / `maxToolCalls` / `timeoutSecs` | Contract bounds: the child stops cleanly after the cap with a clear reason. |
| `disallowedTools` | Tools the child must never receive. |
| `capabilityCeiling` | Per-capability ceiling (`read`/`write`/`exec`); a role that sets only `read: true` gets a strictly read-only tool set. Orchestration plumbing (`todo`/`process`/`task`/`hub`/`goal`) is always kept so a read-only role can still delegate and be supervised. |

Model resolution precedence: settings override → first matching definition
pattern → parent session model (`resolve_agent_model`,
`orchestration/definitions.rs:318-375`). Settings entries can disable an
agent (`settings.agents.<name>.enabled = false`; `/agents` manages these),
and a child that declares tools outside the supported set is not blocked:
unknown declared tools are silently ignored (OMP-compatible) with a single
deduped warning, and an invalid model makes the agent unavailable with an
actionable error.

## The `task` tool

`task` starts one or more independent child coding-session jobs and returns
immediately with stable job and agent ids; supervision happens through `hub`
(`orchestration/tools.rs:39-67`). A child that owns a canonical Todo DAG item
receives that item's stable id as `todoTaskId`.

```text
task agent=researcher task="Study the persistence layer" todoTaskId=task-abc
task tasks=[{name: "w1", task: "Draft API"}, {name: "w2", task: "Write tests"}]
```

Delegation intent is also recognized from the main prompt: an English
delegation verb ("Have researcher study this") or a conservative CJK
construction ("你让researcher仔细调研…") escalates to an exact trusted agent
name; informational mentions do not. Ambiguous mentions with explicit
delegation intent are errors (actionable, naming the candidates).

The matcher is exact-token based (`runtime.rs:4936-4977`): the English
token set is `have, ask, tell, get, let, make, please, delegate, assign,
spawn, run, send, kick, dispatch` (`ENGLISH_DELEGATION_VERBS`,
`runtime.rs:4939-4941`), matched as whole NFKC-lowercased tokens — so
`researchers` never matches `researcher`. The CJK token set is `让, 请, 叫,
派, 安排, 委托, 交给` (`CJK_DELEGATION_TOKENS`, `runtime.rs:4948`), matched
conservatively: the token must directly precede the agent name with no
intervening tokenizer boundary (a CJK script run like `你让researcher`), using
the same word-boundary logic the selector applies (`selector.rs:1107-1111`).
A recognized verb or CJK construction plus an unambiguous agent-name mention
selects that agent; multiple distinct agent names with delegation intent is
an error listing the candidates.

## Jobs and supervision

Every delegated child runs as a **job** (`JobSnapshot` in
`orchestration/runtime.rs`): `queued → running → completed | failed |
cancelled | aborted`, with `created_at`/`started_at`/`finished_at`, a
description, an optional `todoTaskId`, a redacted `result`/`error`, and a
`softBudgetExhausted` marker.

- **Concurrency**: at most `orchestration.maxConcurrency` children run at
  once (default 8, semaphore-bounded); recursion depth is bounded by
  `maxRecursionDepth` (default 8).
- **Job retention**: settled jobs and their artifact files
  (`<agent-id>-<job-id>.md` outputs, `<agent-id>-<job-id>.history.json`
  transcripts) are retained up to `DEFAULT_MAX_RETAINED_JOBS = 256` for
  `DEFAULT_RETAINED_JOB_TTL_SECS = 24h`, then pruned
  (`JobRetention` in `orchestration/runtime.rs:32-33`).
- **Idle parking**: idle non-main agents park after `DEFAULT_IDLE_TTL_SECS =
  300s` and are revived on demand (`schedule_idle_park`).
- **Soft budgets** (`JobSoftBudget` in `orchestration/runtime.rs:154-166`,
  settings `orchestration.softBudget`): `maxRequests`, `maxTokens`, and
  `yieldAfter` are all optional and default to unlimited (run-to-completion
  behavior). When a configured limit is reached the child is **not** failed:
  its run stops cleanly after the current turn, the job settles `Completed`
  with the partial result, and both `TaskResult` and `JobSnapshot` carry
  `softBudgetExhausted: true` so the parent can decide whether to continue
  the child.
- **Contract bounds**: `maxTurns`, `maxToolCalls`, and `timeoutSecs` from
  the agent definition stop the child cleanly with a clear reason; a child
  that exceeds its timeout contract after an abort is reported.
- **Durable orchestration**: child sessions are recorded to their own JSONL
  transcripts under the durable child root; the runtime persists agent
  snapshots, mailboxes, and job state to a sidecar so a restarted session
  revives queued children and re-delivers their mailboxes
  (`orchestration/persistence.rs`).

The child's tool set is assembled per role: coding tools filtered by the
capability ceiling and `disallowedTools`, plus orchestration plumbing. The
child also receives the `yield` tool, the `hub` tool, and the `goal` tool.

## The `hub` tool

`hub` coordinates with Main and child peers (`orchestration/tools.rs:70-103`):

- `hub send <to> <message>` — deliver a mailbox message (subagent ⇄ subagent
  included). Delivery is durable: the message is committed to the recipient's
  mailbox before any revival claim, and the bounded delivered-message log
  (cap 200) keeps messages visible to the workflow page's Recent IRC even
  after consumption.
- `hub wait [from] [timeoutMs]` — block until a message arrives; registered
  waiters make the delivery bridge defer matching sends so the waiter drains
  them durably (`MessageWaiter`, RAII-unregistered on every return path).
- `hub inbox [peek]` — read queued messages without necessarily consuming.
- `hub list` — refresh the peer roster (spawn-time snapshot with
  `<peer_roster>` + `<truncated />` bounds).
- `hub read_history <agent> [lines]` — rendered transcript of a peer
  (default 50 lines, hard max 200, 32 KiB byte cap).
- `hub jobs` / `hub cancel` / `hub wait` — supervise child task jobs.

The roster a child sees is a spawn-time snapshot: `hub list` refreshes state
and `hub send` addresses exact ids.

## The `yield` tool

`yield` is the explicit-delivery protocol for child sessions
(`orchestration/tools.rs:104-119`, `YieldState` in
`orchestration/runtime.rs:791-836`): a child calls it exactly once, passing
the full final deliverable as `text`; that payload becomes the job's delivered
output and the child's run terminates. It is wired to per-run delivery state
the run loop reads when the child settles, so the payload lands in
`TaskResult.output`. A child that exits without calling `yield` settles with
its natural final text plus the marker:

```text
SYSTEM WARNING: Subagent exited without calling yield
```

## The `goal` tool

Children receive the `goal` tool so a delegated worker can create, show,
pause, resume, complete, or drop a session goal — the same durable goal
state machine documented in [`goals.md`](goals.md). Orchestration plumbing
means role filters never remove it.

## Live progress and activity

The runtime publishes `ApplicationEvent`s (`agent_start`, `agent_settled`,
`job_*`, `todo_updated`, orchestration mailbox messages) that drive the TUI
agents panel and the workflow page. Each agent has a live `AgentSnapshot`
(display name, status, current task summary, todo ownership) and a bounded
per-agent activity feed; the workflow detail merges delegated task lifecycle
entries with IRC messages per agent (`workflow/detail.rs`). Everything shown
to the UI is redacted.

## Concurrency gates and settings

- `orchestration.maxConcurrency` (default 8) — child semaphore; a global
  workflow-scoped gate can additionally limit workflow children.
- `orchestration.maxRecursionDepth` (default 8) — the `task` tool is only
  offered to a child while `depth < maxRecursionDepth`.
- `orchestration.mailboxCapacity` (default 1000) — per-agent mailbox bound.
- `orchestration.maxToolsPerAgent` (default 16) — tool-count ceiling for a
  child's coding tools.
- `orchestration.sandboxed` (default false) — run child process spawns in the
  Linux filesystem sandbox.
- `orchestration.softBudget` — per-child soft budget (see above).

All bounds are validated in `OrchestrationConfig::validate`
(`orchestration/runtime.rs:368-392`) and at settings load time
(`settings.rs:2674-2697`).

## Invariants

- A child job is never failed by a soft budget — it settles `Completed` with
  the partial result and the `softBudgetExhausted` marker.
- `yield` delivers exactly once; a missing `yield` is observable to the
  parent via `MISSING_YIELD_WARNING`.
- Messages are durably committed to the mailbox before any revival claim, so
  a restart cannot lose a delivered IRC message.
- Mailbox waits are RAII: every return path (timeout, abort, shutdown, drop)
  unregisters the waiter so no stale claim strands a message.
- Role ceilings only ever remove tools; orchestration plumbing
  (`todo`/`process`/`task`/`hub`/`goal`) is preserved so restricted roles
  can still delegate and be supervised.

## Related documentation

- [`workflows.md`](workflows.md) — workflow lifecycle on top of this runtime
- [`todos.md`](todos.md) — the Todo DAG and its execution
- [`goals.md`](goals.md) — the durable goal state machine
- [`skills.md`](../reference/skills.md) — agent definition format details
