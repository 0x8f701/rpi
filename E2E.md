# rpi End-to-End Verification

This handbook defines the release verification contract for `rpi` 0.2.7. It uses repository-relative paths and isolated temporary homes. Do not record local absolute paths, private endpoints, credential filenames, or credential values in committed evidence.

All prose, fixtures, agent definitions, skill bodies, and faux responses used by deterministic campaigns are English-only.

## Product contract

- Primary executable: `rpi` (JSONL RPC control plane via the `rpi rpc` subcommand).
- Managed install root: `~/.rpi` by default, overridden by `PI_HOME`.
- Active executable: `~/.rpi/bin/rpi` on Unix and `%USERPROFILE%\.rpi\bin\rpi.exe` on Windows.
- Runtime configuration remains compatible with the upstream Pi layout: `~/.pi/agent`, project `.pi/`, and `PI_*` environment variables.
- Native sessions use append-only Pi v3 JSONL.
- Release assets use `rpi-<version>-<target-triple>.tar.gz`, or `.zip` for Windows.
- The five release targets are:
  - `aarch64-apple-darwin`
  - `aarch64-unknown-linux-gnu`
  - `x86_64-apple-darwin`
  - `x86_64-pc-windows-msvc`
  - `x86_64-unknown-linux-gnu`

## Prerequisites and isolation

Every deterministic lane MUST:

- Build or point at a release-dist binary: `target/release-dist/rpi` (override with `RPI_BIN`).
- Use isolated homes and workspaces under `$WORK_ROOT/<scenario>/{home,workspace}` (defaults via `E2E.d/lib/common.sh`).
- Evidence under `$EVIDENCE_ROOT/<scenario>/` (default `${TMPDIR:-/tmp}/rpi-e2e-evidence/<run-id>`).
- Run offline faux: `PI_OFFLINE=1`, `PI_SKIP_VERSION_CHECK=1`, `--offline --model faux/faux-1`.
- Bound wall clocks with `timeout` (see per-lane tables). Hard-fail on timeout.
- Avoid credentials, private endpoints, and local absolute paths in committed content.
- Clean up via `cleanup_e2e` trap: kill registered tmux sessions and PIDs; remove `$WORK_ROOT`.

Shared helpers:

| Path | Role |
| --- | --- |
| `E2E.d/lib/common.sh` | Roots, isolated `rpi`, timeouts, tmux session registry, cleanup |
| `E2E.d/lib/orchestration_fixtures.sh` | Trusted researcher/writer agents, research skill, settings, PNG fixture, `tmux_wait_for` |
| `E2E.d/lib/workflow_fixtures.sh` | Workflow settings/agents, git workspace seed, compact/master-detail/settings asserts, product API probe |
| `E2E.d/lib/run_rpc_campaign.py` | Core RPC lifecycle driver |
| `E2E.d/lib/run_orchestration_campaign.py` | Orchestration RPC driver (goal/todo/process/NL prompts) |
| `E2E.d/lib/run_workflow_campaign.py` | Multi-workflow RPC driver (concurrent create, worktrees, ownership, integrate/conflict) |
| `E2E.d/lib/run_todo_tui_campaign.py` | Dense 21-item Todo tmux driver with unsent composer sentinel |
| `E2E.d/lib/run_bash_tui_campaign.py` | OpenAI-compatible Bash tool tmux driver for stdin/prompt ownership |
| `E2E.d/lib/assert_jsonl.py` | JSONL shape assertions |

List every registered scenario:

```sh
bash E2E.d/list.sh
```

## Verification lanes

### Deterministic CI

Run the checked-in release gate:

```sh
bash E2E.d/ci.sh
```

This lane uses isolated homes, the faux provider, recorded fixtures, and bounded command timeouts. It covers:

- `rpi --version`
- JSON lifecycle output
- LF-delimited RPC commands and responses
- command discovery with exactly 22 primary slash commands, including `/workflow`
- foreground bash
- process supervision and `/ps`
- loop scheduling
- goal state (objective/status/tokens/time)
- authoritative todo DAG readiness + coordinator execution (`todo_dag_execution`)
- TUI Todo refresh after tree/fork/new-session (`replace_transcript_refreshes_todo_phases`)
- session naming, tree, and state
- orchestration umbrella: `orchestration.rpc` + `orchestration.rust` (`cargo +1.88.0`); `orchestration.tmux` when `tmux` is available — hard compact `Task N agents` card, NL exact vs skill-only, bidirectional IRC (rust), exact `/goal`/`/ps`
- trusted QuickJS extension commands (`E2E_CI_EXTENSION=1`)
- installer static regression checks
- optional geometry matrix when `E2E_CI_TMUX=1`

The CI lane must not read live provider credentials.

Focused entrypoints:

```sh
bash E2E.d/ci/campaigns.sh list
bash E2E.d/ci/campaigns.sh run
bash E2E.d/ci/campaign.sh list
bash E2E.d/ci/campaign.sh run
bash E2E.d/ci/orchestration.sh list
bash E2E.d/ci/orchestration.sh run
bash E2E.d/ci/workflow.sh list
bash E2E.d/ci/workflow.sh release      # hard release gate: requires tmux; rpc + tmux + goal-tmux
```

### Release archive fixture

Build the release binary and exercise every archive shape:

```sh
cargo +1.88.0 build --package pi-cli --bin rpi --profile release-dist --locked
RPI_BIN=target/release-dist/rpi bash E2E.d/release/archive-fixture-smoke.sh run
```

The archive gate verifies:

- exactly five platform archives
- exact `rpi-0.2.7-<target-triple>` names
- a single root-level `rpi` or `rpi.exe`
- a root-level `LICENSE`
- a complete and valid `SHA256SUMS`

### Installer and self-update fixture

```sh
RPI_BIN=target/release-dist/rpi bash E2E.d/release/install-self-update.sh run
```

This lane starts a loopback release fixture and verifies download, checksum validation, smoke testing, atomic activation, managed legacy `pi` cleanup, unmanaged `pi` preservation, and current-version self-update behavior.

### Headless Web listener release smoke

```sh
cargo +1.88.0 build --package pi-cli --bin rpi --profile release-dist --locked
RPI_BIN=target/release-dist/rpi bash E2E.d/release/listen-web-smoke.sh run
```

This focused release-binary gate starts `rpi --listen` with closed stdin,
proves it stays alive and serves the embedded `/web` page plus tokenless
`/rpc`, rejects TUI/cursor-probe output, submits a Web RPC prompt, stops on
SIGTERM, restarts with `--continue`, and verifies the recorded user and
assistant turns are restored from the normal session store.

### Live-model lanes

Live-model tests are manual or nightly only:

```sh
RPI_BIN=target/release-dist/rpi \
RPI_LIVE_MODEL='<provider/model>' \
RPI_LIVE_API_KEY_ENV='<credential-environment-variable-name>' \
bash E2E.d/nightly.sh run
```

The environment variable named by `RPI_LIVE_API_KEY_ENV` must already be set by the operator. The scripts must never print or persist its value.

Live scenarios cover architecture research, a build-and-fix task, running-child steering, and compaction overflow. Live results are evidence, not substitutes for deterministic release gates.

## Focused Rust gates

The separate hosted Test workflow (`test.yml`) gates focused Rust 1.88.0 checks
for workspace compilation, provider and agent contracts, coding tools, todo DAG
behavior, process/session lifecycles, structured output modes, trusted QuickJS
extensions, self-update, installers, and the release-dist binary. The release
workflow (`release.yml`) invokes reusable `test.yml` before validating the tag,
building archives, and publishing them, so those tests run against the exact
tag commit.

Before a tag is created, also run the complete library suites serially, plus the Codex provider loopback integration tests (the only tests covering `openai-codex-responses`):

```sh
cargo +1.88.0 test -p pi-ai --lib --locked
cargo +1.88.0 test -p pi-ai --test codex_transport --locked -- --test-threads=1
cargo +1.88.0 test -p pi-agent --lib --locked -- --test-threads=1
cargo +1.88.0 test -p pi-coding --lib --locked -- --test-threads=1
cargo +1.88.0 test -p pi-cli --lib --locked -- --test-threads=1
```

Orchestration-focused cargo gates are also driven by `bash E2E.d/ci/orchestration.sh rust` (see [Regression matrix](#regression-matrix)).

## Scenario catalog

### Core CI campaigns (`E2E.d/ci/campaigns.sh`)

| Scenario ID | Command | Timeout | Evidence root | Pass criteria (observable) |
| --- | --- | --- | --- | --- |
| `campaign.version` | `bash E2E.d/ci/campaigns.sh run` (or internal `run_version`) | 30s | `$EVIDENCE_ROOT/version/version.log` | `rpi --version` succeeds under isolated home |
| `campaign.faux-json` | same umbrella | 30s | `$EVIDENCE_ROOT/faux-json/{events.jsonl,stderr.log}` | JSON lifecycle contains `deterministic-e2e-reply` |
| `campaign.rpc-state` | same umbrella | 40s | `$EVIDENCE_ROOT/rpc-state/{output.jsonl,stderr.log}` | RPC responses succeed for models, commands, bash, todos, todo-state, goal, goal-get, loop, loops, spawn, process-list, state, name, tree; goal objective `deterministic release readiness`; todo task blockedBy inventory; ≥1 process |
| `campaign.orchestration` | `bash E2E.d/ci/campaigns.sh orchestration` or umbrella `run` | see orchestration | nested under orchestration evidence | `todo_dag_execution` + NL/IRC both ways + hard compact-agent tmux + TUI Todo refresh gate; rpc+rust always; tmux when available |
| `campaign.bash-tui` | `bash E2E.d/ci/campaigns.sh bash-tui` or umbrella `run` | 90s | `$EVIDENCE_ROOT/bash-tui/{tui.txt,assertions.json}` | foreground Bash sees EOF, unattended prompt/pager environment reaches the child, the tool turn completes, and an unsent composer sentinel remains editable |
| `campaign.workflow` | `bash E2E.d/ci/campaigns.sh workflow` (dev umbrella) or `bash E2E.d/ci/workflow.sh release` (**hard release gate**, tmux required; **opt-in**, not default `ci.sh`) | see workflow | nested under workflow evidence | Multi-workflow RPC and tmux contracts below; pass requires `workflow.rpc`, `workflow.tmux`, and `workflow.goal-tmux` execution statuses all `passed` (`RPI_BIN=target/release-dist/rpi bash E2E.d/ci/workflow.sh release`, exit 0, evidence under `$EVIDENCE_ROOT`); `workflow campaigns passed` is printed only then |
| `campaign.extension` | `E2E_CI_EXTENSION=1 bash E2E.d/ci.sh` or `bash E2E.d/ci/campaigns.sh extension` | tmux sleeps ~5s + command | `$EVIDENCE_ROOT/extension/{tui.txt,tui.ansi}` | pane contains `alpha:hello` and `beta:two` |
| `campaign.tmux-matrix` | `E2E_CI_TMUX=1 bash E2E.d/ci.sh` or `bash E2E.d/ci/campaigns.sh tmux-matrix` | ~6s per size | `$EVIDENCE_ROOT/tmux-{90x31,120x31,163x40}/{tui.txt,tui.ansi,metadata.txt}` | each size captures `matrix input probe` |

### Focused RPC campaigns (`E2E.d/ci/campaign.sh`)

```sh
bash E2E.d/ci/campaign.sh list
bash E2E.d/ci/campaign.sh run              # all
bash E2E.d/ci/campaign.sh process|loop|goal|todo|tools|session
```

| Scenario ID | Evidence | Assertion |
| --- | --- | --- |
| `campaign.process` | `$EVIDENCE_ROOT/campaign-process/output.jsonl` | supervised process spawn/list through RPC succeeds |
| `campaign.loop` | `$EVIDENCE_ROOT/campaign-loop/output.jsonl` | scheduled loop create/list succeeds |
| `campaign.goal` | `$EVIDENCE_ROOT/campaign-goal/output.jsonl` | goal create/get succeeds |
| `campaign.todo` | `$EVIDENCE_ROOT/campaign-todo/output.jsonl` | dependency-blocked todo projection succeeds |
| `campaign.tools` | `$EVIDENCE_ROOT/campaign-tools/output.jsonl` | command catalog + deterministic foreground bash |
| `campaign.session` | `$EVIDENCE_ROOT/campaign-session/output.jsonl` | name/state/tree session lifecycle succeeds |

All RPC responses in a scenario MUST have `"success": true`. Timeout: 40s.

### Orchestration campaigns (`E2E.d/ci/orchestration.sh`)

```sh
bash E2E.d/ci/orchestration.sh list
bash E2E.d/ci/orchestration.sh run          # rpc + rust + tmux (tmux skipped if missing)
bash E2E.d/ci/orchestration.sh rpc
bash E2E.d/ci/orchestration.sh rust
bash E2E.d/ci/orchestration.sh tmux
# aliases via campaigns.sh:
bash E2E.d/ci/campaigns.sh orchestration
bash E2E.d/ci/campaigns.sh orchestration-rpc
bash E2E.d/ci/campaigns.sh orchestration-rust
bash E2E.d/ci/campaigns.sh orchestration-tmux
```

| Scenario ID | Command | Timeout | Evidence | Authoritative? |
| --- | --- | --- | --- | --- |
| `orchestration.rpc` | `bash E2E.d/ci/orchestration.sh rpc` | 90s outer / 35s RPC client | `$EVIDENCE_ROOT/orchestration-rpc/{output.jsonl,stderr.log,summary.json,rpc-rows.jsonl}` | goal details, todo **readiness projection**, supervised process RPC (not coordinator execution) |
| `orchestration.rust` | `bash E2E.d/ci/orchestration.sh rust` | 240–300s per `cargo +1.88.0` test | `$EVIDENCE_ROOT/orchestration-rust/cargo.log` | NL spawn, `todo_dag_execution`, IRC both ways, image placeholder, TUI Todo refresh, `/ps` PTY, routing — **`cargo +1.88.0` only** |
| `orchestration.tmux` | `bash E2E.d/ci/orchestration.sh tmux` | ~45s interactive + wait helpers | `$EVIDENCE_ROOT/orchestration-tmux/*` (see table below) | exact `/goal`/`/ps`; **HARD** compact-agent-only capture; separate 21-item Todo pressure lane; Goal and Todo unsent composer sentinels; raw XML forbidden |
| `orchestration.run` | `bash E2E.d/ci/orchestration.sh run` | sum of above | `$EVIDENCE_ROOT` | Umbrella |

Trusted fixtures installed by `prepare_orchestration_home` (via `E2E.d/lib/orchestration_fixtures.sh`):

- Settings enable `orchestration.process/tasks/todo`, `maxConcurrency=4`, `maxRecursionDepth=2`, `selector.autoSelectThreshold=0`.
- User agents: `researcher` (read/grep/hub), `writer` (read/write/hub).
- Overlapping skill: `research` with body `RESEARCH_BODY for skill-only routing checks.`
- Faux reply: `deterministic-orchestration-reply`.

#### `orchestration.rpc` checks

Driver: `E2E.d/lib/run_orchestration_campaign.py`. **Projection only** — coordinator execution, abort suppression, restore auto-spawn, and failed/cancelled-open are authoritative in `orchestration.rust` `todo_dag_execution`.

| Check ID | Observable contract |
| --- | --- |
| `goal-details` | `goal_get.current.objective == "deterministic orchestration readiness"`; `lifecycle == "active"`; `usage.tokensUsed == 42`; `usage.activeTimeSeconds == 7`; `tokenBudget == 1000` |
| `todo-ready-roots-and-blocked-join` | After `set_todos` with phases Roots(`root-a`,`root-b`) + Join(`join` depends on both): `root-a.ready` and `root-b.ready` are true; `join.ready` is false; `join.blockedBy` taskIds are exactly `{root-a, root-b}` |
| `todo-exact-task-ids` | Projected task ids are exactly `{join, root-a, root-b}` (`summary.todoInitial.exactIds`) |
| `todo-exact-task-id-completion` | Completing only `root-a` yields completed count 1; join still not ready (exact task-id ownership surface) |
| `todo-dependent-after-roots` | Completing `root-b` makes `join.ready == true`, clears `blockedBy`, completed count 2; `joinStatus` must **not** be `completed` yet |
| `todo-open-work-remains-after-partial` | After roots complete, join remains open work (not spuriously settled) |
| `todo-open-work-remains` | At least one task remains `pending` or `in_progress` after partial completion |
| `todo-blocked-only-projection` | Alternate graph: one ready `gate` + blocked `waiter`; waiter blockedBy gate only (no dual ready roots) |
| `todo-all-terminal-no-ready` | All-terminal projection (`done-a`,`done-b`) has no ready open tasks (`summary.todoTerminal.ids`) — attach-no-spawn surface |
| `process-list-contains-supervised` | `process_spawn` of `sh -c "printf 'orchestration-server-ready\\n'; sleep 120"` yields an id present in `process_list` |
| `process-stopped-and-cleaned` | After `process_stop`, that id is not still `running`/`starting`/`alive`/`active` |
| `nl-prompts-issued` | RPC issues prompts `Have researcher study this` then `Use research for this` (spawn/no-spawn authority is the rust gate) |

Failure: missing check id in `summary.json`, goal/todo field mismatch, wrong exact ids, join completed early, or process still running → non-zero exit.

#### `orchestration.rust` cargo gates

All invoked from `run_orchestration_rust` with **`cargo +1.88.0` only** (never bare `cargo`) into `$EVIDENCE_ROOT/orchestration-rust/cargo.log`.

**Required log needles (fail-closed; post-FF0F `todo_dag_execution` surface):** `todo_dag_execution`, `two_ready_roots_overlap_and_join_waits_for_both_before_three_of_three`, `failed_and_cancelled_owners_stay_open_and_terminal_reconciliation_is_idempotent`, `nl_exact_agent_spawn`, `main_supervises_two_children_with_irc`, `message_delivered_event_renders_once`, `replace_transcript_refreshes_todo_phases`.

| Cargo invocation | Contract under test |
| --- | --- |
| `cargo +1.88.0 test -p pi-cli --test nl_exact_agent_spawn --locked -- --nocapture` | Exact NL `Have researcher study this` → AgentUpdated + JobUpdated with human name `researcher` and job status in `{Queued, Running, Completed}`; Subagents job cards non-empty with researcher display name. Skill-only `Use research for this` does **not** increase job count |
| `cargo +1.88.0 test -p pi-coding --test routing_contracts --locked -- --nocapture` | Exact agent mention wins over overlapping skill; skill-only / ambiguous / untrusted skill failures stay actionable |
| `cargo +1.88.0 test -p pi-coding --test todo_dag_lifecycle --locked -- --nocapture` | Todo tool DAG add/update/dependency/readiness/complete/remove; model-visible ready/blocked fields; on-disk `todo_snapshot` restore |
| `cargo +1.88.0 test -p pi-coding --test todo_dag_execution --locked -- --nocapture` | **Full coordinator suite first** (additions never silently dropped) |
| `cargo +1.88.0 test -p pi-coding --test todo_dag_execution two_ready_roots_overlap_and_join_waits_for_both_before_three_of_three --locked -- --nocapture` | Ready roots overlap; exact `todoTaskId` ownership; join waits for both; 3/3 complete |
| `cargo +1.88.0 test -p pi-coding --test todo_dag_execution failed_and_cancelled_owners_stay_open_and_terminal_reconciliation_is_idempotent --locked -- --nocapture` | Failed/cancelled owners leave tasks open; Blocked status; terminal reconciliation idempotent |
| `cargo +1.88.0 test -p pi-cli formats_empty_goal_details_for_overlay --locked -- --nocapture` | Goal details overlay formatter (Objective/Status/Tokens/Time) |
| `cargo +1.88.0 test -p pi-cli orchestration_irc_renders_named_label_body_reply --locked -- --nocapture` | Human IRC `IRC · Main → Child` / `IRC · Child → Sibling` with bodies `hello child` / `child ack` and `reply to m1`; **no** raw `<orchestration-message` and no `Replying to message` |
| `cargo +1.88.0 test -p pi-cli message_delivered_event_renders_once_and_dedupes_custom_message --locked -- --nocapture` | Child→Main (and peer) MessageDelivered path renders once; custom-message dedupe; no double body |
| `cargo +1.88.0 test -p pi-coding --test orchestration_supervision main_supervises_two_children_with_irc --locked -- --nocapture` | Runtime bidirectional IRC while Main supervises two children |
| `cargo +1.88.0 test -p pi-cli clipboard_png_fixture_attaches_one_image --locked -- --nocapture` | Composer attachment label is exactly `[Image #1, WIDTHxHEIGHT]` (1×1 PNG fixture); draft text preserved; no base64/mime leak |
| `cargo +1.88.0 test -p pi-cli replace_transcript_refreshes_todo_phases_from_application --locked -- --nocapture` | After tree/fork/new-session `replace_transcript_from_application`, `todo_phases` MUST match `application.todo_state()` — no stale Todo panel chrome from the prior DAG |
| `cargo +1.88.0 test -p pi-cli --test process_ps_pty --locked -- --nocapture` (unix) | PTY: `/process start sleep 60`; per-key `/ps` keeps full `/ps` (not `/s`); panel opens; unknown key keeps panel; Esc → Ready; second `/ps`; stop; no orphan |

Failure: any cargo +1.88.0 test non-zero, or missing required log needles → scenario fails; last 200 log lines printed. Full suite timeout 360s; filtered runs 300s each.

**Not currently gated (post-FF0F):** restore auto-spawn, all-terminal no-spawn attach, abort/transition mutation suppress, no-orchestration mid-transition Ready block, navigate drain clears `transition_active`, late terminal invalidated generation — see Known gaps until product re-adds tests.

#### `orchestration.tmux` captures

Session geometry: 120×40. Evidence files under `$EVIDENCE_ROOT/orchestration-tmux/`:

| Evidence file | How produced | Required visible strings / transitions |
| --- | --- | --- |
| `boot.txt` | wait up to 20s for `Ready` | First paint includes `Ready` (soft: wait may time out without hard fail) |
| `goal-typed.txt` | per-key `/` `g` `o` `a` `l` | MUST contain `/goal`; MUST NOT contain `/goall` |
| `goal-panel.txt` | Enter after bare `/goal` | At least one of: `Goal`, `Show details`, `no goal`, `Create goal` |
| `goal-details.txt` | `/goal create --tokens 500 deterministic orchestration readiness` then bare `/goal` + Enter for details | At least one of: `Objective:`, `deterministic orchestration readiness`, `active` |
| `pre-subagents.txt` | capture before exact NL spawn | Prefer empty Todos; if `Todos ·` present, scenario attempts `/todo` clear before spawn |
| `subagents-exact.txt` | after `Have researcher study this` (wait ≤25s) | **HARD:** MUST contain compact `Task N agents` chrome and `researcher`; MUST match lifecycle token `queued\|running\|completed\|parked\|idle` (case-insensitive); MUST NOT contain `Todos ·` |
| `subagents-skill-only.txt` | after `Use research for this` | `researcher` count must not jump by more than +1 vs prior capture (anti-spawn-flood) |
| `todo-input.txt` | buffer source (**after** compact-agent-only assert) | Hierarchical markdown loaded into composer — separate from the agent-only path |
| `todo-panel.txt` | after `/todo` paste | At least one of: `Todos ·`, `fetch inventory`, `compile crate`, `ship release`, `Roots` |
| `ps-typed.txt` | per-key `/` `p` `s` | MUST contain `/ps` |
| `ps-panel.txt` | Enter after `/ps` following `/process start sh -c 'printf orchestration-server-ready; sleep 90'` | At least one of: `Processes`, `sleep`, `running`, `Running`, `orchestration-server`, `sh -c` |
| `tui.txt` / `tui.ansi` | final full scrollback | MUST lack `<orchestration-message`; MUST show Goal markers; MUST match at least one of `Task N agents`, `Processes`, `Todos ·` |
| `irc-meta.txt` | written by driver | `irc_tmux=raw_xml_forbidden`; `irc_authoritative=rust:orchestration_supervision+message_delivered+orchestration_irc` |
| `clipboard-meta.txt` | optional xclip path | `clipboard_png=attempted` or `clipboard_png=skipped` + reason. Authoritative image contract is rust `clipboard_png_fixture` |

Cleanup: `tmux kill-session` for the unique session name; outer `cleanup_e2e` removes work roots.

### Workflow campaigns (`E2E.d/ci/workflow.sh`)

**Execution status: passed for 0.2.7.**
The full workflow campaign passed for 0.2.7 with `workflow.rpc`,
`workflow.tmux`, and `workflow.goal-tmux` execution statuses all set to
`passed` (evidence under `$EVIDENCE_ROOT`). Releases re-verify with the hard
`release` mode below: tmux is required, and `workflow campaigns passed` is
printed only after all three lanes record `execution_status=passed`.
Workflow remains an explicit release lane rather than part of default
`bash E2E.d/ci.sh`.

```sh
bash E2E.d/ci/workflow.sh list
bash E2E.d/ci/workflow.sh release      # HARD release gate: requires tmux; rpc + tmux + goal-tmux
bash E2E.d/ci/workflow.sh run          # developer umbrella (tmux lanes skipped if tmux missing)
bash E2E.d/ci/workflow.sh rpc
bash E2E.d/ci/workflow.sh tmux
bash E2E.d/ci/workflow.sh goal-tmux
# aliases via campaigns.sh:
bash E2E.d/ci/campaigns.sh workflow
bash E2E.d/ci/campaigns.sh workflow-rpc
bash E2E.d/ci/campaigns.sh workflow-tmux
```

| Scenario ID | Command | Timeout | Evidence | Authoritative? |
| --- | --- | --- | --- | --- |
| `workflow.rpc` | `bash E2E.d/ci/workflow.sh rpc` | 120s outer / 45s RPC client | `$EVIDENCE_ROOT/workflow-rpc/{output.jsonl,stderr.log,summary.json,rpc-rows.jsonl,execution-status.txt}` | concurrent create, separate worktrees, ownership Todo roots, supervisors+IRC ownership, pause/resume/cancel idempotent, clean integrate, explicit conflict visible |
| `workflow.tmux` | `bash E2E.d/ci/workflow.sh tmux` | ~45s interactive + wait helpers | `$EVIDENCE_ROOT/workflow-tmux/*` (see table below) | compact header; `/workflow` master-detail; settings overlay excluded from scrollback; unsent workflow composer sentinel remains editable |
| `workflow.goal-tmux` | `bash E2E.d/ci/workflow.sh goal-tmux` | 180s outer / bounded waits | `$EVIDENCE_ROOT/workflow-goal-tmux/{assertions.json,execution-status.txt}` | exact Chinese goal/workflow commands; real Todo calls/workers; four distinct workflow DAGs; multi-DAG Todo detail with phases, tasks, linked jobs |
| `workflow.release` | `bash E2E.d/ci/workflow.sh release` | sum of lanes | `$EVIDENCE_ROOT` | **Hard gate**: tmux required; `workflow.rpc`, `workflow.tmux`, and `workflow.goal-tmux` must all record `execution_status=passed` before `workflow campaigns passed` is printed; absence or failure of any lane exits non-zero |
| `workflow.run` | `bash E2E.d/ci/workflow.sh run` | sum of lanes (tmux lanes skipped if missing) | `$EVIDENCE_ROOT` | Developer umbrella; never prints the complete-pass claim while a required lane was skipped |

Wire contract (product-owned; E2E asserts only):

- Status enum: `queued|planning|running|paused|integrating|completed|failed|cancelled|conflicted` (snake_case)
- `/workflow` bare opens page; subcommands: `create <name> <objective>`, `list`, `show [id|name]`, `pause`, `resume`, `cancel`, `integrate`, `remove`
- RPC: `workflow_create`, `workflow_list`, `workflow_get`, `workflow_pause`, `workflow_resume`, `workflow_cancel`, `workflow_integrate`, `workflow_remove`
- Ownership: explicit camelCase `WorkflowTaskOwnership` `{workflowId, todoTaskId}` (never text inference)
- Compact normal header: `Workflows · {A} active · {T} total`
- Wire snapshot (`WorkflowWireSnapshot`): `workflowId`, `name`, `objective`, `status`, `generation`, redacted `worktree` basename label (never absolute path), `branch`, `baseCommit`, `supervisorAgentId`, `supervisorJobId`, `failure`, `integration`
- Domain worktree branch namespace: `rpi/workflow/<workflowId>` (+suffix on collision); managed root outside source tree; creation fail-closed
- Events: `workflow_updated` / `workflow_status_changed` carry `workflowId` + `generation`

Trusted fixtures installed by `prepare_workflow_home` (via `E2E.d/lib/workflow_fixtures.sh`):

- Settings enable orchestration process/tasks/todo plus `workflow.enabled`, `maxConcurrent=4`, `worktree=true`, `failClosedWorktree=true`, `maxRecursionDepth=2`, `selector.autoSelectThreshold=0`
- User agents: `supervisor` (depth-1), `worker` (depth-2), plus `researcher`/`writer` for shared routing fixtures
- Overlapping skill: `research` body `RESEARCH_BODY for skill-only routing checks.`
- Faux reply: `deterministic-workflow-reply`
- Git seed via `prepare_workflow_git_workspace` (init + single commit) so worktree creation can bind

#### `workflow.rpc` checks

Driver: `E2E.d/lib/run_workflow_campaign.py`.

| Check ID | Observable contract |
| --- | --- |
| `workflow-list-empty-initial` | `workflow_list` succeeds with `data.workflows` empty at start |
| `two-workflows-created-concurrently` | `workflow_create` for `alpha-flow` and `beta-flow` yields distinct `workflowId` values; status ∈ {queued, planning, running}; `generation` is a positive int; wire JSON has no absolute path leak |
| `workflow-list-contains-both` | subsequent `workflow_list` includes both ids and names under `data.workflows` |
| `separate-git-worktrees` | each snapshot exposes a distinct redacted `worktree` label (not absolute); branches differ; branch uses `rpi/workflow/` (or transitional `workflow/`) namespace when present |
| `independent-ready-todo-roots-overlap` | both workflows project ownership-scoped Todo tasks (shared ready-root ids allowed) |
| `cross-workflow-task-ids-no-collision` | composite ownership keys `(workflowId, todoTaskId)` remain unique across workflows even when task ids are identical |
| `supervisors-started-per-workflow` | each workflow snapshot exposes `supervisorAgentId` / supervisor projection |
| `supervisor-irc-directives-owned` | IRC/workflow events that carry `ownership.workflowId` or event `workflowId` only reference known workflow ids |
| `workflow-pause` | `workflow_pause` on alpha → status `paused` |
| `workflow-resume` | `workflow_resume` on alpha → status ∈ {queued, planning, running} |
| `workflow-cancel-idempotent` | create `gamma-flow`, cancel; second cancel keeps durable `cancelled` (success no-op or rejected terminal) |
| `non-conflicting-integration` | `workflow_integrate` on beta succeeds with status ∈ {integrating, completed} and not `conflicted` |
| `explicit-conflict-preserved-visible` | durable `workflow_get` status `conflicted` with visible `integration`/`failure` (via inject or integrate clash path) |
| `workflow-remove` | `workflow_remove` on cancelled gamma; id absent from subsequent list |
| `generation-field-typed-when-present` | `generation` is an integer on lifecycle snapshots |

Failure: missing check id in `summary.json`, field mismatch, absolute path on wire, or `execution_status != passed` → non-zero exit. Missing product APIs write `execution_status=blocked_missing_product_apis` and fail closed (no false pass).

#### `workflow.tmux` captures

Session geometry: 120×40. Evidence files under `$EVIDENCE_ROOT/workflow-tmux/`:

| Evidence file | How produced | Required visible strings / transitions |
| --- | --- | --- |
| `boot.txt` | wait up to 20s for `Ready` | First paint includes `Ready` (soft: wait may time out without hard fail) |
| `pre-settings.txt` | durable transcript seed | MUST contain `workflow transcript anchor line` |
| `settings-open.txt` | after `/settings` Enter | capture while overlay open (diagnostic) |
| `settings-scrollback.txt` | Escape dismiss + continue typing | MUST retain transcript anchors; **HARD** MUST NOT contain `Settings ·`, `Category `, `Ctrl-S apply`, `[settings-open]`, `settings overlay sticky` |
| `normal-compact.txt` | after two `/workflow create` | **HARD** match `Workflows · A active · T total` with total ≥ 2; **HARD** MUST NOT contain full `Todos ·` chrome |
| `workflow-list.txt` / `master-detail.txt` | bare `/workflow`, then Enter on the selected row | **HARD** list contains `alpha-flow` and `beta-flow`; selected detail contains `Objective`, `Status`, `Todos`, `Supervisor`, `Subagents`, `Recent IRC`, `Worktree`, `Integration` |
| `lifecycle.txt` | `/workflow pause\|resume` + `list` | MUST contain `alpha-flow` and `beta-flow` |
| `conflict-meta.txt` | driver note | `conflict_visible=true` or `conflict_visible=deferred_to_rpc` |
| `tui.txt` / `tui.ansi` | final full scrollback | compact header present; settings chrome still absent |
| `execution-status.txt` | driver | `execution_status=passed` only after hard asserts succeed |
| `meta.txt` | driver | documents rpc authority for worktree/IRC ownership |

Cleanup: `tmux kill-session` for the unique session name; outer `cleanup_e2e` removes work roots.

### Live campaigns (`E2E.d/live/campaign.sh`)

| Scenario ID | Command | Evidence |
| --- | --- | --- |
| `live.architecture-research` | `bash E2E.d/manual.sh architecture-research` | `$EVIDENCE_ROOT/architecture-research/{prompt.txt,output.log,stderr.log}` |
| `live.zig-build-fix` | `bash E2E.d/manual.sh zig-build-fix` | `$EVIDENCE_ROOT/zig-build-fix/...` |
| `live.subagent-steering` | `bash E2E.d/manual.sh subagent-steering` | child steered to write `NEW` in `steering.txt` |
| `live.compact-overflow` | `bash E2E.d/manual.sh compact-overflow` | compaction event under soak |

Default live timeout: `LIVE_TIMEOUT_SECONDS` (900s unless overridden).

## Regression matrix

Every user-reported regression maps to an executable lane. Lanes claim **observable behavior**, not source text.

### 1. Command discovery (22 primary slash commands)

**Lane:** RPC + unit/integration + executed workflow RPC/tmux campaign. `/workflow` is one of the 22 primary commands and owns the multi-workflow product surface.

**Commands:**

```sh
bash E2E.d/ci/campaign.sh tools
# or full:
bash E2E.d/ci/campaigns.sh run
cargo +1.88.0 test -p pi-cli --test slash_command_dispatch --locked
cargo +1.88.0 test -p pi-cli --test rpc_binary --locked
```

**Expected visible / structured set (exactly these primary names):**

`settings`, `model`, `branch`, `resume`, `fork`, `export`, `dump`, `handoff`, `agents`, `role`, `persona`, `compact`, `rewind`, `checkpoint`, `ps`, `loop`, `goal`, `workflow`, `code-review`, `btw`, `queue`, `live`

TUI slash discovery MUST surface exactly:

`/settings`, `/model`, `/branch`, `/resume`, `/fork`, `/export`, `/dump`, `/handoff`, `/agents`, `/role`, `/persona`, `/compact`, `/rewind`, `/checkpoint`, `/ps`, `/loop`, `/goal`, `/workflow`, `/code-review`, `/btw`, `/queue`, `/live`

**Evidence:** `$EVIDENCE_ROOT/campaign-tools/output.jsonl` or `$EVIDENCE_ROOT/rpc-state/output.jsonl` (`commands` response).

**Failure:** missing or extra primary command name; RPC success false.


### 2. Subagents status / list

**Lane:** `orchestration.tmux` **HARD** compact-agent-only assert + `orchestration.rust` (`nl_exact_agent_spawn`).

**Commands:**

```sh
bash E2E.d/ci/orchestration.sh rust
bash E2E.d/ci/orchestration.sh tmux
```

**Expected state transitions:**

| Step | Observable |
| --- | --- |
| Before spawn | Capture `pre-subagents.txt`; Todos chrome cleared if present |
| After exact NL `Have researcher study this` | **HARD** compact `Task N agents` chrome + human name `researcher`; lifecycle token ∈ {queued, running, completed, parked, idle} (any case) |
| Agent-only capture | MUST NOT show `Todos ·` in the same capture |
| Job cards (rust) | Agent display name `researcher`; job status ∈ {Queued, Running, Completed} |

**Evidence:**

- Rust: `$EVIDENCE_ROOT/orchestration-rust/cargo.log` (needle `nl_exact_agent_spawn`)
- Tmux: `$EVIDENCE_ROOT/orchestration-tmux/{pre-subagents.txt,subagents-exact.txt}`

**Failure:** missing compact `Task N agents` chrome or `researcher`; missing lifecycle token; `Todos ·` during agent-only capture; rust gate without researcher cards.

### 3. Natural-language exact agent spawn vs skill-only routing

**Lane:** `orchestration.rust` + routing_contracts; tmux anti-flood (**HARD** spawn path).

**Prompts (fixtures English-only):**

| Prompt | Expected routing |
| --- | --- |
| `Have researcher study this` | Exact agent mention → spawn researcher; job cards + Subagents panel populated |
| `Use research for this` | Skill-only → **no** additional subagent spawn |

**Commands:**

```sh
bash E2E.d/ci/orchestration.sh rust
bash E2E.d/ci/orchestration.sh tmux
bash E2E.d/ci/orchestration.sh rpc   # issues both prompts over RPC
```

**Evidence:** `cargo.log`; `subagents-exact.txt`; `subagents-skill-only.txt`; RPC `summary.json` → `nl`.

**Failure:** skill-only increases job count; exact NL fails to spawn; tmux `researcher` count jumps by more than +1 after skill-only.

### 4. Todo DAG waves and status updates

**Lane split:**

- **Readiness projection:** `orchestration.rpc` (`set_todos` waves + exact ids + blocked-only + all-terminal) + `todo_dag_lifecycle` rust + tmux todo panel (**separate** capture after the compact-agent-only assertion).
- **Coordinator execution (authoritative, post-FF0F):** `orchestration.rust` → full `todo_dag_execution` suite **plus** fail-closed filters `two_ready_roots_overlap_and_join_waits_for_both_before_three_of_three` and `failed_and_cancelled_owners_stay_open_and_terminal_reconciliation_is_idempotent` only.

**Commands:**

```sh
bash E2E.d/ci/orchestration.sh rpc
bash E2E.d/ci/orchestration.sh rust
bash E2E.d/ci/orchestration.sh tmux
bash E2E.d/ci/campaign.sh todo
```

**Expected readiness projection wave (RPC — unchanged):**

```
initial:  exactIds={join,root-a,root-b}
          root-a ready=true, root-b ready=true, join ready=false
          join.blockedBy = {root-a, root-b}
after A:  completed=1 (exact task-id completion), join still not ready
after B:  completed=2, join ready=true, blockedBy cleared, joinStatus != completed
open:     join remains pending|in_progress (todo-open-work-remains / after-partial)
blocked:  gate ready + waiter blockedBy={gate} (todo-blocked-only-projection)
terminal: ids={done-a,done-b}, no ready open (todo-all-terminal-no-ready)
```

**Expected coordinator execution (rust `todo_dag_execution` — existing tests only):**

| Filter | Observable |
| --- | --- |
| `two_ready_roots_overlap_and_join_waits_for_both_before_three_of_three` | Ready roots overlap; exact todoTaskId; join waits; 3/3 complete |
| `failed_and_cancelled_owners_stay_open_and_terminal_reconciliation_is_idempotent` | Failed/cancelled stay open; Blocked; reconcile idempotent |

Tmux panel after hierarchical `/todo` paste (post-Subagents) MUST show task text among: `fetch inventory`, `compile crate`, `ship release`, and chrome `Todos ·` or phase `Roots`.

**Evidence:** `$EVIDENCE_ROOT/orchestration-rpc/summary.json` keys `todoInitial`, `todoAfterRoots`, `todoTerminal` + projection checks; `todo-panel.txt`; `cargo.log` needles `todo_dag_execution`, both filter names, `todo_dag_lifecycle`, `replace_transcript_refreshes_todo_phases`.

**Failure:** join ready while a root open; wrong exact ids; join completed early; blocked/terminal projection wrong; tmux missing task markers; rust missing fail-closed needles; stale Todo after `replace_transcript`.

### 5. Auto-continuation / open-work readiness

**Lane (partial):**

- RPC: `todo-open-work-remains`, `todo-open-work-remains-after-partial`, `todo-all-terminal-no-ready`, `todo-blocked-only-projection` (projection surfaces only).
- Rust: `todo_dag_execution` 3/3 complete + failed/cancelled stay open (idempotent reconcile).

**Contract claimed by this handbook:**

- After dependent roots complete in RPC projection, open join work still exists (not spuriously empty).
- Coordinator reaches 3/3 complete when roots + join finish; failed/cancelled do not false-complete owners.
- This does **not** claim restore auto-spawn, abort/transition mutation suppress, mid-transition Ready block, or full product auto-continue Ready-guard (composer never Ready with ready open work).

**Commands:**

```sh
bash E2E.d/ci/orchestration.sh rpc
bash E2E.d/ci/orchestration.sh rust
```

**Evidence:** `summary.json` projection checks; `cargo.log` includes `todo_dag_execution` + two filter needles.

**Gap:** restore auto-spawn / all-terminal no-spawn attach, abort-suppress then resume, transition-Ready mutation block, navigate drain clearing `transition_active`, late terminal generation ignore, and full auto-continue Ready-guard — Known gaps until product re-adds tests/drivers.

### 6. Goal selector / inspect / details

**Lane:** `orchestration.rpc` + `orchestration.tmux` + goal formatter rust gate + `campaign.goal`.

**Commands:**

```sh
bash E2E.d/ci/orchestration.sh rpc
bash E2E.d/ci/orchestration.sh tmux
bash E2E.d/ci/campaign.sh goal
```

**Expected:**

| Surface | Strings / fields |
| --- | --- |
| RPC create/get | objective `deterministic orchestration readiness`; lifecycle `active`; tokensUsed `42`; tokenBudget `1000`; activeTimeSeconds `7` |
| Tmux bare `/goal` | Goal UI: `Goal` / `Show details` / `no goal` / `Create goal` |
| Tmux create | `/goal create --tokens 500 deterministic orchestration readiness` |
| Tmux details | `Objective:` and/or objective text and/or `active` |
| Exact input | Composer shows `/goal` never `/goall` while typing |

**Evidence:** `orchestration-rpc/summary.json` → `goal`; `goal-typed.txt`, `goal-panel.txt`, `goal-details.txt`.

**Failure:** objective/lifecycle/usage mismatch; bare `/goal` opens nothing; `/goall` appears.

### 7. Goal ↔ subagent IRC communication

**Lane:** `orchestration.rust` authoritative for **both directions**; tmux only forbids raw XML and records `irc-meta.txt`.

**Commands:**

```sh
bash E2E.d/ci/orchestration.sh rust
bash E2E.d/ci/orchestration.sh tmux
```

**Expected visible lines (human renderer / MessageDelivered / supervision):**

```
IRC · Main → Child
hello child
IRC · Child → Sibling
child ack
reply to m1
```

Plus runtime gate `main_supervises_two_children_with_irc` and `message_delivered_event_renders_once_and_dedupes_custom_message` for Child→Main / peer delivery without double render.

MUST NOT appear: `<orchestration-message`, `Replying to message`.

**Evidence:** `orchestration-rust/cargo.log` (needles `main_supervises_two_children_with_irc`, `message_delivered_event_renders_once`); tmux `tui.txt` lacks `<orchestration-message`; `irc-meta.txt` documents rust authority.

**Gap:** live Goal↔subagent IRC **body text inside tmux capture** is not hard-asserted; rust owns labels/bodies/reply.

### 8. Clipboard image composer dimensions

**Lane:** rust `clipboard_png_fixture_attaches_one_image` authoritative; tmux optional xclip attempt.

**Commands:**

```sh
bash E2E.d/ci/orchestration.sh rust
bash E2E.d/ci/orchestration.sh tmux   # optional injection only
```

**Expected placeholder format:**

```
[Image #N, WIDTHxHEIGHT]
```

Concrete 1×1 fixture: `[Image #1, 1x1]` (or decoded PNG dimensions). Multi-attach preserves `#1`, `#2`, … Draft text around the attachment is preserved. No raw base64 (`iVBORw0KGgo`) and no `image/png` leak in labels.

**Evidence:** `cargo.log`; `clipboard-meta.txt` (`attempted` vs `skipped`).

**Failure:** wrong placeholder shape; rust test fail. Tmux skip without DISPLAY/xclip is not failure.

### 9. Sparse palette / code highlighting

**Lane:** focused Rust unit tests (not orchestration shell). Run via library suite or direct filters.

**Commands:**

```sh
cargo +1.88.0 test -p pi-cli screenshot_markdown_keeps_prose_default_and_cyan_sparse --locked -- --nocapture
cargo +1.88.0 test -p pi-cli dark_palette_matches_observed_omp_titanium_values --locked -- --nocapture
cargo +1.88.0 test -p pi-cli fenced_code_uses_multiple_semantic_token_styles --locked -- --nocapture
cargo +1.88.0 test -p pi-cli assistant_markdown_matches_shared_neutral_output_for_rich_blocks --locked -- --nocapture
```

**Expected:**

- Ordinary prose spans are **not** cyan (`md_heading` accent).
- Cyan spans remain sparse: `cyan_spans * 3 < visible_spans`.
- Fenced code emits multiple semantic token styles (keyword/function/string/…).
- Dark palette matches locked OMP titanium RGB values (accent `00b4ff`, etc.).

**Evidence:** cargo test output (CI library suites). No `$EVIDENCE_ROOT` folder unless operator redirects.

**Note:** Not registered under `orchestration.*`. Do not claim `bash E2E.d/ci/orchestration.sh` covers palette.

### 10. User message no-indent

**Lane:** focused Rust unit test on transcript user cards.

**Command:**

```sh
cargo +1.88.0 test -p pi-cli user_card_first_glyph_has_no_phantom_prefix_at_normal_and_narrow_widths --locked -- --nocapture
```

**Expected:**

- Prompt `Can you put it in the background?` → first visible glyph is `C` at widths 80 and 10.
- First span content starts with `C` (column zero; no phantom leading indent).
- User card background is `user_message_bg` on all spans.
- Wide emoji row wraps without indent artifacts.

**Evidence:** cargo test output.

### 11. Skills loading / invocation

**Lane:** coding resource + selector unit/integration tests; NL routing overlap covered by orchestration rust.

**Commands:**

```sh
cargo +1.88.0 test -p pi-coding load_skills_and_format --locked -- --nocapture
cargo +1.88.0 test -p pi-coding --test routing_contracts --locked -- --nocapture
cargo +1.88.0 test -p pi-cli --test nl_exact_agent_spawn --locked -- --nocapture
# optional interactive skill reachability:
cargo +1.88.0 test -p pi-coding --test resource_cli_options custom_agent_directory_skill_reaches_interactive_session --locked -- --nocapture
```

**Expected:**

- Trusted skills load from user/project agent dirs; format into Agent Skills prompt block when `read` tool present.
- Exact agent mention suppresses ranked skill autoload for spawn decisions.
- Exact skill mention does not spawn overlapping agent.
- Orchestration fixture skill `research` body remains available for skill-only checks.

**Evidence:** cargo logs; orchestration rust `cargo.log`.

### 12. Supervised background server visible in `/ps`

**Lane:** `orchestration.rpc` process list; `orchestration.rust` `process_ps_pty`; `orchestration.tmux` panel.

**Commands:**

```sh
bash E2E.d/ci/orchestration.sh rpc
bash E2E.d/ci/orchestration.sh rust
bash E2E.d/ci/orchestration.sh tmux
bash E2E.d/ci/campaign.sh process
```

**Expected transitions:**

| Step | Observable |
| --- | --- |
| Spawn | RPC/TUI starts bounded `sh -c`/`sleep` server |
| List | process id ∈ `process_list` / `/ps` panel shows process chrome |
| Exact type | Composer `/ps` full string (p must not drop) |
| Stop | process not left running; PTY cleanup on quit |

Tmux needles (any): `Processes`, `sleep`, `running`, `Running`, `orchestration-server`, `sh -c`.

**Evidence:** `summary.json` checks `process-list-contains-supervised`, `process-stopped-and-cleaned`; `ps-typed.txt`, `ps-panel.txt`; `cargo.log` for PTY.

**Failure:** empty process list after spawn; `/s` without `/ps`; panel missing all needles; process still running after stop.

## TUI and terminal checks

Use the checked-in PTY tests for terminal lifecycle and command behavior. Manual tmux review should use an isolated home and normal-screen scrollback:

```sh
work='<isolated-workspace>'
home='<isolated-home>'
tmux new-session -d -s rpi-e2e -x 120 -y 31 -c "$work" \
  "env HOME='$home' USERPROFILE='$home' PI_CODING_AGENT_DIR='$home/.pi/agent' PI_OFFLINE=1 PI_SKIP_VERSION_CHECK=1 TERM=xterm-256color target/release-dist/rpi --offline --model faux/faux-1"
```

Prefer the scripted lane when asserting orchestration chrome:

```sh
bash E2E.d/ci/orchestration.sh tmux
```

Confirm (manual or scripted):

- slash discovery shows exactly `/settings`, `/model`, `/branch`, `/resume`, `/fork`, `/export`, `/agents`, `/compact`, `/ps`, `/loop`, `/goal`, and `/workflow`
- bare `/goal` shows goal status
- exact `/goal` and `/ps` typing never duplicates the final consonant (`/goall`, dropped `p`)
- single Ctrl-C does not exit an idle TUI
- a second Ctrl-C within 500 ms exits normally
- Ctrl-D exits directly
- printable input is inserted exactly once
- large and multiline paste remains bounded
- Markdown tables, inline code, syntax spans, tool cards, and retained history render correctly
- user messages begin at column zero (no phantom indent)
- sparse cyan accent on headings/links; prose stays default foreground
- clipboard image paste shows `[Image #N, WIDTHxHEIGHT]`
- Subagents list shows human names with queued/running/completed states after exact NL spawn
- Todo panel shows hierarchical tasks and updates completion/readiness
- `/ps` lists supervised background servers
- IRC lines render `IRC · <sender> → <recipient>` with body; no raw orchestration XML

Geometry matrix (optional CI):

```sh
E2E_CI_TMUX=1 bash E2E.d/ci.sh
# or
bash E2E.d/ci/campaigns.sh tmux-matrix
```

Sizes: `90x31`, `120x31`, `163x40`. Each must echo `matrix input probe` in `tui.txt`.

## Filesystem policy checks

The default `read` tool may read ordinary paths outside the current workspace, including absolute paths, parent-relative paths, and symlink targets. This exception applies only to `read`.

Verify that write/edit, sandboxed search, resource discovery, project trust, session import, and `@file` expansion still reject paths outside configured workspace roots or symlink escapes.

## Session and orchestration checks

Verify:

- same-CWD session listing, resume, tree, branch, and fork
- cross-CWD resume switches the complete runtime generation transactionally and preserves the previous runtime on candidate failure (`application_runtime`)
- header-only explicit sessions and forks are materialized
- explicit session IDs are atomically reserved
- session record and reconstructed-message safety limits are enforced
- process IDs and job UUIDs resolve consistently
- running children receive steering exactly once
- cancelled children do not report a false wake
- duplicate agent names receive deterministic suffixes
- todo dependencies remain authoritative across persistence, TUI, and RPC; tree/fork/new-session `replace_transcript` refreshes Todo phases from the application (no stale panel)
- active goals are projected only into model context and remain hidden from human transcripts and exports
- Subagents panel lists only owned children with human-readable names and queued/running/completed states
- exact NL agent mention spawns; overlapping skill-only text does not
- Goal details expose objective, status/lifecycle, token usage, and active time
- Goal ↔ child IRC shows sender → recipient labels and body without raw XML
- supervised processes appear in `/ps` and clean up after stop

Cross-CWD resume is covered by the Rust `application_runtime` integration suite; the deterministic shell campaigns remain same-CWD.

## Structured output checks

JSON and RPC stdout must remain machine-readable and free of ANSI, warnings, debug output, and ordinary diagnostics. RPC uses one JSON object per LF-delimited line. Cancellation must leave the application reusable.

Scheduled and goal system reminders are model-only data. Human TUI, print output, RPC projections, and exports must not show them as user messages.

## Timeouts and failure criteria (summary)

| Lane | Bound | Fail when |
| --- | --- | --- |
| version / faux-json | 30s | non-zero exit; missing reply marker |
| rpc-state / campaign.\* RPC | 40s | any `success != true`; assert script exit |
| orchestration.rpc | 90s / client 35s | missing check ids (incl. exact-ids / blocked-only / all-terminal); field mismatch; join completed early |
| orchestration.rust | 300–360s per `cargo +1.88.0` test | any cargo failure; missing fail-closed needles (`todo_dag_execution`, `two_ready_roots_overlap_and_join_waits_for_both_before_three_of_three`, `failed_and_cancelled_owners_stay_open_and_terminal_reconciliation_is_idempotent`, `nl_exact_agent_spawn`, IRC needles, `replace_transcript_refreshes_todo_phases`) |
| orchestration.tmux | wait ≤20s boot; ≤25s compact agents; fixed sleeps | missing HARD `Task N agents`/researcher needles; `Todos ·` during agent-only capture; `/goall`; skill-only flood; `/ps` empty; raw `<orchestration-message` |
| workflow.rpc | 120s / client 45s | missing product APIs; missing check ids; worktree/ownership/conflict contract fail |
| workflow.tmux | wait ≤20s boot; ≤15s workflow list; fixed sleeps | missing compact header; full Todos on normal screen; workflow list names/detail labels absent; settings chrome in scrollback |
| workflow.goal-tmux | 180s outer / bounded waits | `status != passed`; missing check ids; not four distinct workflow ids; <8 worker completions |
| workflow.release | sum of lanes | tmux absent; any lane fails; `workflow campaigns passed` printed without all three `execution_status=passed` |
| extension / tmux-matrix | short fixed sleeps | grep needles missing |
| live | 900s default | timeout; missing artifact contract |
| installer / archive | script-local | inventory or checksum mismatch |

Any `timeout` kill is a hard failure. Evidence directories MUST remain for diagnosis when `EVIDENCE_ROOT` is preserved by the operator; default cleanup removes `$WORK_ROOT` only (evidence path is outside work root unless overridden).

## Cleanup

- Automatic: `E2E.d/lib/common.sh` `cleanup_e2e` on EXIT/HUP/INT/TERM kills tmux sessions in `E2E_TMUX_SESSIONS`, PIDs in `E2E_PIDS`, and `rm -rf` on `E2E_CLEANUP_PATHS` (includes `$WORK_ROOT`).
- Orchestration tmux also `tmux kill-session` at end of scenario.
- Operators MAY keep evidence: set `EVIDENCE_ROOT` to a retained directory before invoking scripts.
- NEVER commit evidence trees, session exports, or credential material.

## Known gaps

| Topic | Status |
| --- | --- |
| Full Todo auto-continuation Ready-guard (composer never Ready with ready open work) | RPC open-work/terminal projections + two coordinator filters documented under matrix §4–5. Dedicated product auto-continue Ready-guard path still stabilizing — keep gap until a verified driver exists. |
| Restore auto-spawn / all-terminal attach no-spawn | **Not gated post-FF0F.** Former filters `attach_orchestration_to_restored_ready_todo_auto_spawns_owner` and `attach_orchestration_to_all_terminal_restore_does_not_spawn` removed from rust fail-closed list until product re-adds tests. RPC still projects `todo-all-terminal-no-ready` only. |
| Abort-suppress / transition-Ready mutation block | **Not gated post-FF0F.** Former filters `transition_active_rejects_direct_mutations_and_explicit_execute`, `no_orchestration_session_transition_still_rejects_direct_todo_mutation`, `navigate_tree_begin_drain_error_clears_transition_active`, `late_terminal_from_invalidated_generation_ignores_same_stable_todo_id` removed until product re-adds tests. |
| Live Goal↔subagent IRC body text inside tmux | Rust IRC gates (both directions) authoritative; tmux asserts absence of raw XML + `irc-meta.txt` only. |
| Clipboard image in tmux | Optional/best-effort; rust fixture authoritative. |
| Sparse palette / code highlighting / user no-indent | Covered by focused `cargo +1.88.0` tests, not `orchestration.sh`. |
| Skills loading beyond routing | Resource/selector cargo tests; not a separate E2E.d shell scenario. |
| Multi-workflow campaign (`E2E.d/ci/workflow.sh release`) | Hard gate: requires tmux; `workflow campaigns passed` only after `workflow.rpc`, `workflow.tmux`, and `workflow.goal-tmux` all record `execution_status=passed`; absence or failure of any lane fails. Passed for 0.2.7 (all three statuses `passed`). Default `E2E.d/ci.sh` includes the user-perspective workflow TUI scenario on tmux hosts; the authoritative RPC/tmux/goal lanes run separately (and are gated by the hosted Test workflow). |

## Release checklist

1. Workspace version and lockfile workspace packages are `0.2.7`.
2. `cargo +1.88.0 build --package pi-cli --bin rpi --profile release-dist --locked` succeeds.
3. `target/release-dist/rpi --version` prints `rpi 0.2.7`.
4. Complete library suites pass, including `cargo +1.88.0 test -p pi-ai --test codex_transport --locked` (Codex WS/SSE loopback contracts).
5. `bash E2E.d/ci.sh` passes (includes orchestration umbrella after core campaigns).
6. `bash E2E.d/ci/orchestration.sh run` passes on a tmux-capable host (or rpc+rust with explicit tmux skip log).
7. `RPI_BIN=target/release-dist/rpi bash E2E.d/ci/workflow.sh release` passes on a tmux-capable host — the hard gate: tmux is required, and `workflow campaigns passed` is emitted only after `workflow.rpc`, `workflow.tmux`, and `workflow.goal-tmux` each record `execution_status=passed` under `$EVIDENCE_ROOT`; absence or failure of any lane fails the gate. A plain `workflow.sh run` dev run that skips tmux lanes is not release evidence.
8. The release archive fixture passes.
9. Installer and self-update fixtures pass.
10. README installation commands match `install.sh`, `install.ps1`, and the published archive names.
11. The release commit contains no credentials, local paths, build artifacts, exported sessions, or generated diagrams.
12. Tag `v0.2.7` points exactly at the verified release commit.
    — Tagging is performed only after the release commit's hosted Test workflow passes.
13. Push the release commit, wait for its hosted Test workflow, then push `v0.2.7` to the GitHub remote.
14. Confirm every hosted build and the GitHub Release publication job succeeds.
    — Depends on step 13, which is not performed locally.
15. Confirm regression matrix rows 1–12 either passed or are explicitly listed under Known gaps for this tag.
