# E2E scenarios (user-perspective tmux tests)

This page is the **scenario catalog** for user-perspective, goal-driven E2E
testing of the rpi TUI. Every entry is written the way a user would experience
the product: a **user goal**, the **concrete tmux interaction** that pursues
it, the **observable outcome** on the real TUI, and the **pass criteria** that
prove the goal was achieved. The catalog covers every core feature; the
highest-value scenarios are implemented as runnable, deterministic tmux
scripts under `E2E.d/` (see [Implementation status](#implementation-status)).

## How scenarios are executed

- The binary under test is `target/release-dist/rpi` (`RPI_BIN`).
- Scenarios that need no model tool calls use the built-in faux provider
  (`--model faux/faux-1` + `PI_FAUX_RESPONSE`): fully offline and
  deterministic.
- Scenarios that must exercise tool calls (bash cards, todo DAG creation,
  workflow planning/workers) use a **loopback mock provider** (an
  OpenAI-compatible SSE server on `localhost`), never a real model — still
  offline and deterministic.
- Every script: starts tmux → launches rpi with the scenario's flags/model →
  drives keystrokes → asserts on pane text (status line, panel rows, output)
  and/or file side effects → captures evidence under
  `EVIDENCE_ROOT` → kills the session.
- Scripts are self-contained, skip-guarded (`tmux` present, `RPI_BIN`
  executable, faux model available), and use bounded
  wait-for-pattern polling instead of sleeps wherever possible.
- Run a single scenario or the whole lane:

```sh
# whole lane
bash E2E.d/ci/user_scenarios.sh run
# one scenario
bash E2E.d/ci/user_scenarios.sh goal
# list
bash E2E.d/ci/user_scenarios.sh list
```

Evidence lands in `${TMPDIR:-/tmp}/rpi-e2e-evidence/<run-id>/user-<scenario>/`.

## Scenario catalog

Legend — **script**: implemented in this repo and runnable now; **lane**:
covered by an existing E2E lane (`E2E.d/ci/*.sh` or Rust `tests/`); **manual**:
needs hardware/credentials/network, runs by hand only.

### 1. Goal lifecycle (create / pin / pause / complete + budget + journal)

| | |
|---|---|
| User goal | Keep a long-running session focused on one objective with a token budget, and journal important constraints. |
| Interaction | `/goal create --tokens 100 ship the widget` → `/goal` → `/goal pin keep the release checklist in scope` → `/goal pins` → `/goal pause` → `/goal resume` → `/goal complete` → `/goal`. |
| Observable outcome | Status line reports `Goal work started · active · 0/100 tokens · ship the widget`; a `Goal` details block lands in the transcript (`Objective: …`, `Status: active`, `Tokens: 0 / 100`, `Time spent: …`); the composer header shows the `🎯 Goal 0/100` chip; pause flips the chip to `⏸` and the summary to `paused · …`; pins list as `1. …`; complete flips to `completed · …`; `show` after `drop` reports `no goal`. |
| Pass criteria | Every lifecycle summary contains the exact lifecycle word (`active`/`paused`/`completed`) and budget fraction; the `🎯`/`⏸`/`✓` chip appears and changes; pin text survives `pins`; the goal persists to the agent-dir goal state file. |
| Status | **script** `E2E.d/user/goal_lifecycle.sh` |

### 2. Loop (create / cancel / delete / continue)

| | |
|---|---|
| User goal | Run a recurring prompt on an interval and own its lifecycle. |
| Interaction | `/loop 1h slash keep-alive` → `/loops` → `/loop-update <id> 2h …` → `/loop-delete <id>` / `/loop-cancel <id>`; bare `/loop` shows usage. |
| Observable outcome | `/loop` reports `scheduled <task-id>` and the loop fires immediately (faux reply appears); `/loops` lists the loop with its id and prompt; missing-argument forms print `usage`. |
| Pass criteria | A parseable loop id is printed and echoed by `/loops`; delete/cancel succeed without error text. |
| Status | **lane** `ic.tui-loop` (`E2E.d/ci/interactive_commands.sh`) + Rust `goal_loop_e2e.rs` |

### 3. Workflow (plan → todo → execute → integrate; isolation; restart)

| | |
|---|---|
| User goal | Delegate a complex objective to an isolated, supervised workflow that plans, executes, and integrates its own changes back. |
| Interaction | `/workflow create ship-flow ship the widget` → watch the planning feed → `/todo` (workflow-owned DAG rows) → `/workflow integrate ship-flow` → `/workflow show ship-flow`; concurrently create a second workflow and verify isolation; restart rpi and verify the workflow resumes. |
| Observable outcome | Creation prints the workflow detail block (`Status: queued` → live events move it through `planning` → `running`); the compact header counts `Workflows · N active · M total`; the supervisor's planning turns create real todo phases/tasks visible on the Todo DAG page; integration applies the workflow branch (`Integration: applied · <commit>`), status becomes `completed`; each workflow lives in its own git worktree; a restarted session restores non-terminal workflows. |
| Pass criteria | Two concurrent workflows never share a worktree or collide on task ids; integration produces a real merge commit on the source branch; pause/resume/cancel/remove transitions are reflected in `show`. |
| Status | **script** `E2E.d/user/workflow_full_run.sh` (create → plan → todo → execute → integrate → completed); **lane** `workflow.goal-tmux`, `workflow.tmux`, `workflow.rpc`, `D11RestartRecovery` |

### 4. Orchestration (subagents spawn/delegate, IRC, soft budgets, yield)

| | |
|---|---|
| User goal | Have the main session delegate work to named subagents, receive their progress over IRC, and park them on a soft budget. |
| Interaction | Natural-language delegation ("Have researcher study this") → observe the agent card and `👥` footer count → supervisor `yield` releases the turn; subagent completion lands in the transcript; IRC directives route to the owning workflow. |
| Observable outcome | The subagent card renders agent name/status; the footer shows `👥 N` while agents run; `⟦N bg⟧` appears on the status line; yield tool parks the agent and returns control; a soft-budget-exhausted job surfaces in its card. |
| Pass criteria | Delegation selects the trusted agent by name; cards appear and resolve; `yield` observably ends the subagent turn without error. |
| Status | **lane** `orchestration.rpc` / `orchestration.rust` / `orchestration.tmux` (`E2E.d/ci/orchestration.sh`), `D47YieldTool`, `T50SoftBudgets` |

### 5. Todo DAG (list / detail / execute)

| | |
|---|---|
| User goal | Maintain a phased task list and inspect the dependency DAG before letting the model execute it. |
| Interaction | Seed markdown: `/todo # Survey\n- [ ] map parser surface\n# Construct\n- [/] repair composer repaint` → `/todo` opens the DAG page → Enter opens detail → Esc back → Esc closes. |
| Observable outcome | Overview shows `Todo DAGs` with a `[main]` row projecting `open N active M blocked K`; detail renders phases, tasks, `◌`/`●` markers, `in progress` labels and counts; Esc steps detail → overview → closed; composer focus restored. |
| Pass criteria | Exact overview/detail chrome strings appear; the seeded task names and phase names render; panel closes without leaving overlay state. |
| Status | **script** `E2E.d/user/todo_dag.sh`; **lane** Rust `core_tui_e2e.rs::pty_todo_overview_detail_navigation`, `orchestration.rpc` |

### 6. `/btw` side chat

| | |
|---|---|
| User goal | Run a parallel side conversation without disturbing the main session. |
| Interaction | `/btw` opens the overlay → type a prompt, Esc closes (session kept) → reopen and confirm persistence → `/btw new alpha` → `/btw list` → `/btw alpha` → `/btw close alpha`. |
| Observable outcome | Status reports `Side chat open · tab … · Esc closes overlay (session kept)`; tab create reports `Side chat · tab alpha created · N of M tabs open`; `/btw list` shows tabs with the active marked `▸`; the side conversation persists across close/reopen. |
| Pass criteria | Overlay opens/closes with status transitions; tab list contains created names; the main composer still accepts input after close. |
| Status | **script** `E2E.d/user/btw_side_chat.sh`; **lane** Rust `core_tui_e2e.rs::pty_btw_side_chat_open_paste_esc_reopen_persist` |

### 7. `/live` voice (hold-to-talk)

| | |
|---|---|
| User goal | Dictate a prompt by holding a key. |
| Interaction | `/live` arms hold-to-talk; Ctrl+Space records; transcript lands in the composer for review before Enter. |
| Observable outcome | Status line shows `⟦live⟧` while armed; recorded text appears in the composer, not the transcript. |
| Pass criteria | Manual: requires a working microphone; the TUI never blocks without one. |
| Status | **manual** (no mic on CI); **lane** `L1LiveVoice`, `docs/src/user-guide/live.md` |

### 8. MCP (stdio server connect + tool call)

| | |
|---|---|
| User goal | Attach an external MCP stdio server and use its tools. |
| Interaction | Register an MCP server (config/`--mcp`), prompt the model to call its tool, verify the tool result card. |
| Observable outcome | Server connects (`/mcp` status), tool call appears as a card with the server-provided result. |
| Pass criteria | Tool result text from the MCP server lands in the transcript; disconnect cleans up. |
| Status | **lane** `M1McpGateway`, `D44McpAcpTests` |

### 9. ACP (agent stdio session)

| | |
|---|---|
| User goal | Drive rpi from an external agent over the ACP stdio protocol. |
| Interaction | Launch rpi under ACP (`--mode acp`), exchange `initialize`/`session/…` messages, approve a tool call via `session/request_permission`. |
| Observable outcome | Protocol handshake succeeds; tool approval round-trips; completion returns the model text. |
| Pass criteria | Deterministic envelope + tool approval exchange without host credentials. |
| Status | **lane** `A1AcpProtocol`, `D44McpAcpTests`, Rust `acp_stdio_e2e.rs` |

### 10. Extensions (overlay open, plugin install from dir/git, trust)

| | |
|---|---|
| User goal | Load a third-party extension, open its overlay, and install a plugin from a directory or git source. |
| Interaction | Launch with `--extension <dir>`; `/run alpha hello` and `/chain alpha one | beta two`; `/overlay <id>`; `rpi install <dir|git>` with `--approve`. |
| Observable outcome | `/run` prints `alpha:hello`; `/chain` pipes outputs; the overlay opens over the TUI; installing an extension from a git URL lands in the extension dir. |
| Pass criteria | Command/chain outputs appear in the pane; untrusted installs fail closed without `--approve`. |
| Status | **lane** `campaign.extension`, `ic` overlay/plugin lanes, `D45OverlayP0`, `D50GitPluginSource`, `G10ExtensionDesign` |

### 11. `/rewind` + `/snapcompact` + `/compact`

| | |
|---|---|
| User goal | Roll a session back to a checkpoint and shrink its context without losing the archive. |
| Interaction | Run a few prompts → `/checkpoint mid` → `/rewind` (picker lists indices and `[checkpoint mid -> …]`) → `/rewind <entry>` → `/snapcompact` → `/compact`. |
| Observable outcome | The picker shows entry indices, types and checkpoint annotations; a rewind reports `rewound to entry N (kept …, dropped … record(s)); archived tail to <path>` and writes a `.rewind-*.jsonl` sidecar; `/snapcompact` reports `Compacted X → Y estimated tokens` on the status line (A→B) and writes a `.snapcompact-*.jsonl` archive; `/compact` (LLM summarizer) reports `Compaction complete`. |
| Pass criteria | Transcript before the rewind target disappears from the live view; the archived tail file exists and contains the dropped records; the A→B token status shows a strict decrease. |
| Status | **script** `E2E.d/user/rewind_compact.sh` (checkpoint, picker, rewind + sidecar, `/snapcompact` A→B status + sidecar); `/compact` LLM summarizer completion covered by unit + Rust REPL lanes (`rewind_checkpoint_snapcompact_e2e.rs`); `T102RewindCheckpoint` |

### 12. `/handoff`

| | |
|---|---|
| User goal | Produce a handoff summary another agent can act on. |
| Interaction | `/handoff` → `/handoff --prose`. |
| Observable outcome | A `Handoff` transcript block renders the deterministic envelope (`# Handoff`); the clipboard copy runs in the background; `--prose` in the TUI stays envelope-only with a hint that prose is a REPL/CLI surface. |
| Pass criteria | Envelope text appears in the pane; the summarizer is never invoked on the TUI event loop. |
| Status | **script** `E2E.d/user/steering_queue_handoff.sh`; **lane** Rust `handoff_prose.rs`, `T99HandoffProse` |

### 13. `/fresh`, `/dump`, `/share --encrypt`

| | |
|---|---|
| User goal | Start over cleanly, export the session, and share it privately. |
| Interaction | `/fresh` (alias `/new`) starts a new session; `/dump [--jsonl] [path]` writes an HTML/JSONL export; `/share --encrypt [passphrase]` writes an AES-256-GCM `.jsonl.enc` (never touches the network without a gist seam). |
| Observable outcome | `/fresh` prints `Started a new session` and `/name` reads `(unnamed)`; the dump file exists on disk with the session content; the encrypted share file exists and its header identifies the cipher. |
| Pass criteria | File side effects verified on disk; `/share` honors a fake `gh` seam and never requires host credentials. |
| Status | **lane** `ic.tui-name-new` (`/new`), `ic.tui-export` (`/export`), `ic.tui-share` (`/share` + fake gh), `T82FreshDumpShare`, `T38ExportFlagHardening` |

### 14. Hooks / trust

| | |
|---|---|
| User goal | Run a trust hook that upgrades an untrusted project decision. |
| Interaction | Install a hooks config with a `trust_decision` handler; launch in an untrusted project; verify the hook's `approve` recommendation is applied and its veto is never (a hook cannot weaken a stored denial). |
| Observable outcome | The hook's approval lets the project resource load; a `deny`-only payload stays inert. |
| Pass criteria | Hook event fires and the decision changes exactly as the fail-open contract allows. |
| Status | **lane** `T27HooksSystem`, `T91TrustHook`, `T98TrustWiring`, `docs/src/reference/settings-trust.md` |

### 15. Sandbox (bash denied path)

| | |
|---|---|
| User goal | Ensure a denied filesystem path stays out of reach of the model's bash. |
| Interaction | Configure sandbox denials; prompt a bash call against the denied path; observe the tool error. |
| Observable outcome | The bash tool card renders the denial error; no file is created/read on the denied path. |
| Pass criteria | The tool result contains the denial reason; the denied path is untouched on disk. |
| Status | **lane** `O1OsIsolation`, `D10SandboxOverlay`, `D15SandboxToolsE2e`, Rust `sandbox` tests, `docs/src/reference/sandbox-isolation.md` |

### 16. Image generation (faux)

| | |
|---|---|
| User goal | Generate an image from a prompt. |
| Interaction | `/image <prompt>` with a faux-capable provider. |
| Observable outcome | The image placeholder card renders `[Image #N, WxH]` (or the real image under kitty graphics). |
| Pass criteria | API surface exercised against a faux generator (no real model); real-image rendering marked manual. |
| Status | **manual** (real generation); **lane** `G1ImageGen`, `T109ImageInspect`, Rust `write_orchestration_png_fixture` |

### 17. Eval / notebook (python cell)

| | |
|---|---|
| User goal | Run an inline python cell in the session. |
| Interaction | `/run python3 -c …` or the notebook overlay; verify cell output in the tool card. |
| Observable outcome | Cell stdout appears in the card; errors surface as tool errors, not TUI hangs. |
| Pass criteria | Output text lands in the pane; the composer stays responsive. |
| Status | **lane** `E1EvalNotebook`, `T48OfficeNotebook` |

### 18. Memory tool

| | |
|---|---|
| User goal | Persist a fact across sessions with the memory tool. |
| Interaction | Prompt the model to store a fact (mock tool call) → verify the memory file → new session reads it back. |
| Observable outcome | Memory file updated; a follow-up turn's context includes the stored fact. |
| Pass criteria | File side effect + cross-session retrieval. |
| Status | **lane** `T74MemorySystem`, `T87SkillGoalPins`, `docs/src/reference/skills.md` |

### 19. Ask tool

| | |
|---|---|
| User goal | Answer a mid-task question the model asks. |
| Interaction | Mock provider calls `ask`; the status line shows `⟦ask⟧ <question> ⟦esc⟧`; the next submitted line is routed back as the answer. |
| Observable outcome | The pending question is visible above the composer and the answer arrives in the next model request. |
| Pass criteria | Status-line ask glyph appears; the answer text reaches the provider request. |
| Status | **lane** `T51AskTool`, Rust steering/ask unit tests |

### 20. Auto-mode

| | |
|---|---|
| User goal | Let the model classify the task and route it automatically. |
| Interaction | Submit a prompt in auto mode; observe the classification hint (`Detected: code task — /todo …`) and the routed workflow/todo behavior. |
| Observable outcome | The classifier hint renders on the status line; execution follows the routed path. |
| Pass criteria | Hint text appears; routing matches the fixture's classification. |
| Status | **lane** `T76AutoMode`, `T53AutoIntegrate`, `U7WorkflowFixes` |

### 21. `/queue` + doom-loop

| | |
|---|---|
| User goal | Keep steering the agent while it works, then clear the backlog. |
| Interaction | Submit a prompt (mock streams slowly) → type a second prompt while the turn is in flight (follow-up queued) → observe `⚙ N` header count and `Next: /queue · /goal` suggestion → `/queue` lists `Pending prompts: … steering, … follow-up — /queue cancel clears them` → turn completes and the follow-up drains automatically → `/queue` → `Queue is empty`; repeat with `/queue cancel`. |
| Observable outcome | The pending count appears in the composer header (`⚙ 1`), the suggestion line advertises `/queue`, the queue view lists previews, and consumption clears the indicators without a restart. |
| Pass criteria | Counts appear while queued, drain after processing, and `cancel` empties the queue with `Cancelled N queued prompts`. |
| Status | **script** `E2E.d/user/steering_queue_handoff.sh`; **lane** Rust `steering_rpc_binary_e2e.rs` + TUI queue unit tests |

### 22. TUI chrome (theme, status line, bash cards, code frames, list colors, steering queue)

| | |
|---|---|
| User goal | Recognize and control the interface itself: visual theme, live status line, tool cards, and code frames. |
| Interaction | `/theme` / `/theme next`; drive a bash tool call (multi-line command with leading `#` comments) → card renders `╭── # comment ──╮` frame, `$` command rows, ` Output ` separator; prompt a code fence that never closes → bottom border carries `… (unclosed fence)`; queue a steering message → `⟦steering⟧ <preview>` status line; check list/status colors under the active theme. |
| Observable outcome | Theme switch changes the rendered palette; the bash card keeps its 20-row budget with comment frame and `$` rows; the open fence renders `╰── … (unclosed fence) ──╯` instead of a borderless tail; the steering preview sits above the input until the queue drains. |
| Pass criteria | Exact chrome strings (`╭── #`, `$ `, ` Output `, `… (unclosed fence)`) appear in captures; theme cycles; steering glyph and preview appear/clear with queue lifecycle. |
| Status | **script** `E2E.d/user/bash_card_fence.sh` (bash card + unclosed fence) and `E2E.d/user/steering_queue_handoff.sh` (steering line); **lane** `ic.tui-theme`, `K1ThemeAudit`, `F7ThemeListColors`, `T32NoColorFix` |

## Implementation status

| Scenario | Script | Model | Coverage |
|---|---|---|---|
| Goal lifecycle | `E2E.d/user/goal_lifecycle.sh` | faux | full lifecycle + budget chip + pins |
| Rewind / snapcompact / compact | `E2E.d/user/rewind_compact.sh` | faux | picker, checkpoint, sidecar, A→B status |
| `/btw` side chat | `E2E.d/user/btw_side_chat.sh` | faux | open/type/esc/reopen, tabs, list, close |
| Steering queue + `/handoff` | `E2E.d/user/steering_queue_handoff.sh` | mock (slow stream) | follow-up queue, `⚙`/suggestion, drain, cancel, handoff envelope |
| Bash card + code fence | `E2E.d/user/bash_card_fence.sh` | mock (tool calls) | multi-line bash card, comment frame, unclosed fence marker |
| Todo DAG page | `E2E.d/user/todo_dag.sh` | faux | markdown seed, overview/detail chrome, Esc navigation |
| Workflow full run | `E2E.d/user/workflow_full_run.sh` | mock (planning) | create → plan → todo rows → integrate → completed, worktree |

All other features are covered by the lanes listed in the catalog; `/live`
voice and real image generation are manual.

## Verification

First full green run (2026-08-08): `bash E2E.d/ci/user_scenarios.sh run` → exit 0.
Re-verified on the post-web-wave tree (2026-08-09, freshly rebuilt `target/release-dist/rpi`,
rust-toolchain 1.88.0): `bash E2E.d/ci/user_scenarios.sh run` → exit 0.

| Scenario | Result | Evidence |
|---|---|---|
| goal-lifecycle | pass | status/detail/chip captures in `$EVIDENCE_ROOT/<run-id>/goal-lifecycle/` |
| rewind-compact | pass | rewind + snapcompact sidecars asserted on disk |
| btw-side-chat | pass | overlay chrome + tab lifecycle captures |
| steering-queue-handoff | pass | ⚙ counts, auto-drain, `/queue cancel`, handoff envelope in transcript + fake-xclip capture |
| bash-card-fence | pass | comment frame, `$` rows, ` Output ` separator, `… (unclosed fence)` |
| todo-dag | pass | overview/detail chrome + Esc navigation |
| workflow-full-run | pass | create → plan → Todo DAG → workers → auto-integrate → completed; `e2e plan` commit + `PLAN.e2e` verified in git |
| project-authoring | pending | empty git workspace → multi-module Rust CLI written, planted marker-parse defect caught by failing `cargo test`, read+edit repair, passing tests (12/12), valid/invalid CLI runs, Todo DAG completed; driven by `E2E.d/lib/project_authoring_mock.py` |

Notes from verification:

- Scenario scripts must run against a **current** release-dist binary; the
  previously shipped one (Aug 5) predated `/goal pin` and reported
  `a current goal already exists` on pin.
- The status text set by a command that starts a model turn
  (`Goal work started …`) is immediately superseded by the turn's busy label,
  so the scripts assert the details block, the header chip, and the
  post-turn statuses instead.
- `/snapcompact`'s `Compacted A → B estimated tokens` status can be
  overwritten by the session's `Compaction complete` event (event ordering);
  the scripts accept either and assert the sidecar archive durably.
- The non-snap `/compact` LLM summarizer requires `> keepRecentTokens` of
  context before it has anything to compact, which a short faux session cannot
  reach; that surface stays covered by unit tests and the Rust REPL lanes.
- The workflow supervisor prompt reads `You plan workflow …`; loopback mocks
  must classify on that wording, and multi-tool-call responses must stream one
  `tool_calls` index per SSE delta.
