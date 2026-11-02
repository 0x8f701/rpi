> ⛔ **MANDATORY — READ FULL AGENTS.md EVERY 20 MINUTES.** Re-read this entire
> file top-to-bottom at least once every 20 minutes of active work. Violations
> are immediate rejection. No exceptions, no skipping.

# AGENTS.md — rpi (pi-rs)

Rules for AI agents working in this repository. Adapted from the OMP/pi
coding-agent conventions with rpi-specific structure, test commands, and
repository invariants.

## Twelve Hard Constraints

Violating any of these anywhere results in immediate rejection — fix first, continue second.

1. **Re-read this FULL file every 20 minutes of active work** (see the mandatory banner above). Working from a stale memory of these rules is treated the same as violating them.
2. **No TODOs or placeholders in main branch.** `TODO`, `<placeholder>`, standalone question-mark placeholders, `tbd`, and "待补" must not land; idiomatic language syntax such as Rust's `?` operator is allowed.
3. **No inference as conclusion.** Every judgment must trace to `<file>:<line>` with executable verification. Never use "probably", "looks like", "probably should" as a conclusion.
4. **No local absolute paths or secrets in code, comments, docs, or test fixtures.** Only repo-relative paths (`<repo-root>/...`) or explicit placeholders (`<workspace>/...`). No `/home/...`, `/mnt/...`, `~/.ssh`, API keys, tokens, cookies, private endpoints, or internal IPs.
5. **Never push changes.** Do not run `git push` under any circumstances.
6. **Never auto-format.** Do not run formatters (e.g. `cargo fmt`, `prettier`, `black`, `gofmt`) or perform style-only reformatting unless explicitly requested.
7. **Never rollback with `git checkout --`.** Do not discard local changes via `git checkout --`, `git restore`, or equivalent. Rollbacks require manual review and explicit user action.
8. **Never expose credentials, tokens, API keys, or environment variables. Credential use is indirect-only.** Do not read, echo, cat, or output the contents of secret files (e.g. `.env`, `.env.*`, `*.pem`, `*.key`, `auth.json`, credentials files) or environment variables (`env`, `printenv`, `echo $VAR`); never log or display sensitive values. When a credential must be USED (never displayed), it is read indirectly inside a trusted local script or tool that keeps the value in memory (e.g. a token file read into a variable and passed as a header) — never interpolated into commands, URLs, prompts, arguments, or docs where it could render. At ALL times a credential must not be printed, placed into the conversation or any model context, or uploaded to any cloud service (search queries, fetched URLs, error/telemetry reports included). If a task cannot proceed without exposing one, state what is missing instead.
9. **Never access cloud service credentials in HOME.** Do not read, list, or access `~/.aws/`, `~/.gcloud/`, `~/.azure/`, `~/.config/gcloud/`, `~/.config/azure/`, `~/.kube/`, or any other cloud provider credential directories under the home directory.
10. **Prefer agent-browser over curl/wget for web content.** When accessing web pages, use the `agent-browser` tool to render and extract page content instead of `curl`, `wget`, or raw HTTP requests. This saves token usage by getting structured page data rather than dumping raw HTML.

11. **Select the remote shell explicitly for SSH commands.** Never rely on the remote account's login shell to parse automation commands. Invoke the intended interpreter explicitly: use `ssh <host> /usr/bin/bash -s` for Bash scripts or `ssh <host> /usr/bin/fish -c '<command>'` for Fish commands. This prevents quoting, expansion, startup-file, and syntax differences in the remote environment from changing command behavior.

12. **Release-only tests and debugging.** NEVER execute Cargo tests, product binaries, TUI/Web listeners, E2E, gdb/lldb/DAP, or any other runtime or debug session from the default `dev`/`debug` profile. Tests MUST use `cargo +1.88.0 test ... --release` (or `--profile release-dist` for deployed/production-shaped verification). Launch and attach only `target/release` or `target/release-dist` artifacts. Debug-profile observations are not evidence. `cargo check` (type-check only, no execution) remains allowed.

## Repository Layout

- `crates/pi-ai` — model providers (anthropic/openai/responses/faux/imagegen), catalog, stream/retry/timeout.
- `crates/pi-agent` — agent loop: `Agent`, `AgentTool`, `AgentEvent`, tool-call reconciliation.
- `crates/pi-coding` — the coding agent: `Session` (turn execution), `Application` (state machine), tools (`tools.rs` + `tools/*`), orchestration (subagents, durable runtime), extensions (QuickJS + process), workflow (worktree/YAML DAG), storage (`session_store.rs`), settings, auth/trust/sandbox, memory, MCP, goal/loop.
- `crates/pi-cli` — the TUI (`tui.rs` + panels), REPL, JSON-RPC modes (`rpc.rs`, `acp.rs`), subcommands, themes, terminal images (kitty protocol).
- `FEATURES.md` — committed inventory of shipped user-visible capabilities. Update it in the same change that adds, removes, or changes a capability.

## Core Engineering Principles

1. Prefer simple, readable, and low-error code over cleverness, micro-optimizations, or minimizing line count.
2. Keep control flow flat. Minimize `if-else`; prefer `match`/`switch`/early-return structures. Do not exceed 3 levels of nesting.
3. Return early on errors. For `Option`, prefer `ok_or` or `ok_or_else` to produce readable errors and return immediately. For `Result`, use `?` or explicit `match` then early return.
4. Prioritize reuse. Extract shared logic into functions, traits, or small utilities instead of copy-pasting. Abstract only when the same logic appears ≥2 times and will be referenced again.
5. **Strong types over string matching.** New or modified business logic models states, commands, error categories, lifecycle phases, identities, wire variants, and UI modes with enums/tagged unions/newtypes/structured fields; exhaustive `match`/`switch` owns branching. It MUST NOT parse, compare, prefix-test, regex-test, or substring-match human-readable messages, labels, CSS text, titles, or error prose. At an external untyped boundary, one centralized parser may convert raw strings into a closed typed value; all downstream code consumes only that type. When touching an existing prose-matched branch, migrate that branch and every directly coupled caller in the same change; do not leave a parallel old parser. Unrelated untouched prose-matched code stays out of scope until it is touched — it is not a kept `legacy` path for the current change.
6. **Comments are rare and dense.** Use clear names, types, and structure first. Comments are allowed only for invariants, non-obvious reasons, external-spec citations, or known correctness hazards. A straightforward function, test, branch, loop, data transformation, or sequence of calls gets zero explanatory comments: never annotate it line-by-line, section-by-section, step-by-step, or with prose that says what the next statement does. Never add narrative headings inside a function, restate names/control flow, explain obvious assertions, or write signature-filling doc blocks. If understanding needs a paragraph, simplify/extract/rename the code; put longer rationale in review output, not source. Across each changed file, new/modified comment lines must stay at or below 5 per 1,000 changed code lines; a change below 1,000 code lines gets at most 5 comment lines total. Only an irreducible safety invariant or external-spec citation may exceed this, and only when a shorter name/type/assertion cannot carry the same information.
7. Do not work around problems by weakening requirements, silently dropping behavior, or simplifying tasks beyond what was requested.
8. **Single optimal version only.** One behavior has one implementation. Do not keep `canonical` vs alternate, `fallback`, `legacy`, `compat`, `v1`/`v2`, old+new, or dual protocol/scheme/parser/cwd/message paths for the same contract. When a better version exists, delete the superseded path in the same change — breaking changes are required. Retrying the same typed contract is not a fallback. A fail-closed reject is not a fallback. Filesystem `canonicalize_*` helpers (`canonicalize_child_path`) are path operations, not dual implementations. Identifiers, comments, and docs must not introduce `canonical`, `fallback`, or `legacy` except those path helpers. Long-term maintainability takes priority over preserving old interfaces.
9. **Solve only the current problem.** Do not design data structures, functions, abstractions, or interfaces for hypothetical future needs. Implement what is needed now with the simplest correct approach.
10. **Agent Execution Discipline — no divergence.** Concentrate exclusively on completing the assigned task, including work required for correctness and its named acceptance criteria. Do not investigate or act on tangential bugs, speculative improvements, unrelated cleanup, or optional redesign; briefly report unrelated findings without modifying affected files, then continue the assigned objective.
11. **TUI/Web parity.** Shared user capabilities are implemented for the TUI and the web UI within the same task whenever the platform can support them; terms, statuses, information hierarchy, safety bounds, and observable effects must stay similar (for example: compact status glyphs, accessible full state, selection/filter semantics, structured Goal/Constraints/Contract Task cards, typed IRC, Waiting/Thinking states, attachment previews/captions, and safe Markdown rendering). Platform differences are allowed only when deliberate (for example: touch targets, responsive layout, terminal column constraints, or browser media APIs), and each platform's behavior must be verified separately.
12. **Requirements remain cumulative.** Every user-requested task and acceptance criterion remains required and tracked until executable evidence proves completion or the user explicitly cancels or changes it. Append or merge new requests without silently deleting, weakening, rewriting, marking complete, deprioritizing, or displacing earlier requests; if requirements conflict, ask the user instead of choosing.
13. **Fixes before features; features before improvements.** Any user-requested or verified in-scope fix always has higher priority than starting or resuming new feature work. Finish its implementation, focused verification, required different-model review, and narrow commit before feature work continues. Defects outside the active scope are reported and deferred unless the user explicitly brings them into scope; work required to unblock, correct, or validate requested behavior is directly required in-scope work.
14. **Within one priority class, solve easy before hard and bottom-up before top-down.** After applying the mandatory fix → feature → improvement priority, order independent work from the smallest, best-understood, lowest-risk changes to harder cross-module changes. For each dependency chain, repair and verify the lowest authoritative layer first: data/encoding/storage/protocol and runtime ownership before application orchestration, then TUI/Web/API projections and visual behavior. Upper layers must consume verified lower-layer contracts; never patch UI symptoms while the underlying state, persistence, protocol, or lifecycle owner remains incorrect. Independent easy tasks should run in parallel with harder tasks when file ownership and interfaces do not conflict. Dependencies override ease: a simple upper-layer change waits when it requires an unfinished lower-layer contract.
15. **No blocked task states.** Never mark a user requirement, Goal deliverable, Todo item, or verification task as blocked. Exhaust repository tools, safe cleanup, alternate fixtures, focused reproduction, and delegated read-only analysis; keep the item pending or in progress and continue resolving it. External prerequisites must be named precisely while all reachable work continues, never used to stop the active objective.
16. **Exact scope and stopping rule.** Complete every cumulative user instruction and named acceptance criterion, including directly required correctness work, but never enter endless follow-on repair. Do not add adjacent improvements, recursive cleanup, speculative hardening, or optional redesign. Once requested behavior has executable evidence where applicable, and every requested verification, review, documentation, and required cleanup item is complete with appropriate evidence, stop. Further work requires a new user request or a demonstrated defect inside the existing scope. Nothing requested may be omitted, and nothing outside scope may be added.
17. **Evidence-first bug fixing — mandatory pipeline.** Every bug fix follows the full pipeline IN ORDER, and skipping any stage is a rejected fix: (1) REPRODUCE the user-visible failure on the real affected surface (Web: Playwright or agent-browser with inspected screenshot; TUI: real program in tmux with captured pane evidence; capture the actual wire frames/logs/runtime traces where the defect lives); (2) WRITE A FAILING TEST that encodes the reproduction (a deterministic unit/integration/regression test that fails for the exact reported reason — if a real-surface reproduction is genuinely impossible, a test over the real captured fixture/trace is the minimum, and the impossibility is stated, not assumed); (3) FIX the defect at the responsible boundary with root-cause evidence (file:line) — hypotheses and source inspection guide investigation but are never root-cause proof; speculative fixes without captured evidence of the failure mechanism are FORBIDDEN; (4) CROSS-MODEL REVIEW the staged diff with a strictly higher-tier model per the Model Change Cross-Review hierarchy, zero unresolved findings; (5) RE-REPRODUCE on the original surface and confirm the failure no longer triggers, and the failing test now passes. For lifecycle or concurrency bugs, correlate authoritative session/group/tool/job/agent IDs and every state transition through the exact erroneous cancellation or mutation entry. Three failed speculative fixes = stop guessing; capture the real artifact (log, frame, trace, dump) before writing another line.
18. **Core functionality requires dual-surface verification.** Every core user capability and critical lifecycle fix must be exercised and confirmed through both interfaces before completion: Web via Playwright and agent-browser with inspected screenshots, and TUI via the real program in tmux with captured pane evidence. Unit, integration, crate, and workspace tests remain required for code-level contracts but cannot replace either surface. If a capability genuinely cannot exist on one interface, document the concrete platform limitation and verify the closest observable parity path there; never silently waive a surface.
19. **UI verification is an immediate human-journey gate.** Every Web UI change starts with a real rendered reproduction and inspected screenshot, then the current-tree bundle is built and exercised through the exact user click/type/scroll/close journey before the task can be marked complete, reviewed, or committed. Capture and inspect the complete desktop and mobile frames for spacing, geometry, control size, overflow, layering, focus, text hierarchy, errors, and the state after closing or switching away. DOM assertions, source review, unit tests, RPC-only tests, and screenshots captured before the current change cannot satisfy this gate. Every user-reported UI defect must add a permanent real-browser assertion that fails on the reported behavior. Every TUI UI change follows the same sequence against the actual terminal with captured pane evidence.
20. **Release UI matrix uses one current binary.** Before release approval, build one current-tree Web bundle and release binary, then run every changed or user-reported Web journey sequentially against that same binary and isolated production-shaped fixtures. A passing component lane or fake backend does not substitute for the combined journey. Any browser console error, error toast, wrong geometry, stale state, failed command, or screenshot mismatch fails the release candidate and reopens the owning fix.
21. **Converge fast without weakening evidence.** Optimize execution for completion speed and delivered quality, not theoretical 100% soundness. Evidence remains mandatory for every user-agreed acceptance criterion: run the smallest set of checks that proves every agreed criterion and satisfies the existing Testing and applicable UI verification rules, then stop. Implement only critical bug fixes and killer features—failures that break an agreed core behavior, lose or corrupt state, cross a security boundary, deadlock or leak a critical resource, fail compilation, or make required verification falsely pass or fail. Record and defer minor improvements, optional hardening, proof refinements, style cleanup, speculative edge cases, extra abstractions, low-impact optimizations, and test refinements that do not change the agreed outcome unless the user explicitly approves them. Review findings below this material threshold are non-blocking notes, never new work. Reviews get one correction pass for material findings, followed by one final confirmation that those findings are resolved; the change must not land without the required different-model approval. After that confirmation, review may reopen work only for a newly demonstrated critical defect in the agreed scope. Do not create new Todos for deferred findings.


## User Requirements Memorandum

1. Record every user-requested task and acceptance criterion in the repository-root `TASKS.md` as a cumulative local memorandum before implementation.
2. Re-read `TASKS.md` after completing each task so remaining requirements are not lost or silently displaced.
3. `TASKS.md`, `MEMORY.md`, and `PLAN.md` are local working state only. Keep them in `.gitignore` and never stage or commit them.
4. **Periodic MEMORY.md notes.** After each completed node, failed focused check, or 30-minute subagent inspection, append durable notes to repository-root `MEMORY.md`: pitfalls, routing tricks, invariants, and grounded understanding (file:line). Do not write secrets, credentials, or local absolute paths. Treat memory as advisory; current repository files and command output win.
5. **Todo scope requires explicit user agreement.** The Todo list is a shared contract with the user, not an internal scratchpad. Never create, append, split, replace, reorder, or expand Todo items without the user's explicit agreement in the conversation. A newly discovered defect must be handled within an already agreed Todo when it is directly required for that Todo's acceptance; otherwise report and defer it without adding a Todo. Status updates may only mark agreed items started, completed, dropped, or pending according to current evidence; they must not silently change scope.

## Parallel Agent Coordination

1. Before editing, each agent must declare the exact files or modules it owns. Two agents must never edit the same file or implement competing versions of the same feature concurrently.
2. A read-only review assignment forbids edits. A handoff must name the files, current state, remaining acceptance criteria, and explicitly confirm that the previous owner has stopped editing them.
3. **Every subagent task carries an anti-false-evidence contract at creation.** The task text must name owned and forbidden files, observable acceptance, permitted checks, and these mandatory statements: unexecuted tests/builds/browser journeys are `PENDING`; authored assertions or screenshot calls are not execution evidence; a completion message is not acceptance; errors may be ignored only by matching one exact documented non-fatal category while preserving last-good state; old selectors, wrong fixtures, unregistered lanes, timeout/poll waits used as proof, or assertions that do not distinguish the broken behavior are test failures. Missing this contract makes the delegation invalid and the coordinator must stop and reissue it before accepting edits.

   Every delegated task must end with this evidence contract:

   ```text
   # Evidence contract
   - Unexecuted or skipped tests, builds, browser journeys, and screenshots are PENDING.
   - An executed failing check is FAILED, never PENDING or passed.
   - Authored tests, assertion counts, screenshot calls, filenames, and a completion message are not execution evidence.
   - Ignore an error only by matching one exact documented non-fatal category while preserving authoritative last-good state; every other error propagates with context.
   - Old selectors, wrong fixtures, unregistered lanes, timeout/poll waits used as proof, partial scans, and assertions that cannot distinguish the reported bug are test failures.
   - Report exact changed files, raw checks actually run, checks not run, and remaining acceptance gaps. Do not claim completion while any acceptance or current-surface proof is missing.
   ```
4. **Review the diff and raw output, never the subagent's claim.** The coordinating agent reads every delegated diff, checks each broad `catch`, `Err(_)`, `.ok()`, fallback, no-op success, and cleanup failure, and records the exact verification commands. After every participating writer has stopped, the coordinator runs those commands during the centralized verification phase required by rules 12 and 16 and Testing rules 8–10. `complete`, `green`, screenshot filenames, test names, and assertion counts from a subagent are status only until current-tree command output and inspected artifacts prove them. An unexecuted or skipped check remains `PENDING`; an executed failing check is `FAILED`. Neither may be summarized as passed.
5. When an agent discovers adjacent work, it must report or delegate it rather than expanding its own task. Public API, protocol, persistence-format, or architecture changes outside the assigned acceptance criteria require user approval before implementation.
6. **Delegate executable slices, not themes.** A delegated implementation task owns one observable contract, one interface boundary, and normally one to three source files. A request such as “fix session switching,” “fix reconnect memory,” or “implement the backend” is a program of work and must be decomposed before dispatch. Cross-cutting work starts with a small schema/API node; implementation nodes consume that frozen output.
7. **Research once, then share the artifact.** Avoid making every implementer rediscover the same broad call graph. A scout/research node should produce the grounded source map and interface contract when several tasks need it. Later agents receive that artifact, inspect every owned file and any direct or indirect caller/config/test needed for correctness, and avoid repeating open-ended repository-wide exploration.
8. **One primary responsibility per agent.** Keep architecture gates, implementation ownership, QA, and different-model review as distinct nodes. Implementation includes source inspection, authoring the focused tests that defend its contract, diff self-checks, and lightweight compile/type/syntax checks allowed by Testing rule 8; it must report adjacent work instead of silently absorbing a second feature or state machine. Executing tests and runtime behavior remains the coordinator's final verification responsibility.
9. **Use progress checkpoints, not mechanical deadlines.** Long-running silent exploration is prohibited. Agents report concrete progress after the initial investigation and periodically thereafter: files read/changed, interface settled, blocker, and remaining steps. The coordinator splits or redirects a node when evidence shows repeated broad exploration, expanding scope, no reviewable output, or a missing dependency; correctness work is never truncated merely because a fixed number of minutes elapsed.
10. **Prefer vertical slices.** Each implementation node must reach one end-to-end observable boundary: for example, wire type + parser test; runtime identity + one handoff test; pure reducer + reducer tests; capability stream API + atomicity tests. Do not assign several state machines or all layers of a feature to one worker.
11. **Freeze shared interfaces before parallel implementation.** When multiple nodes consume one protocol, persistence format, or state-machine contract, serialize a schema/API design gate first. After it is frozen, assign disjoint backend, frontend, and E2E nodes. Never let sibling agents independently invent both sides of the same wire contract.
12. **No validation against mid-edit shared files.** The coordinator must not run Cargo/npm builds, broad tests, or final review while any active agent is editing a participating file. First obtain an explicit stopped-editing handoff. Mid-edit compile errors are not product evidence and waste the validation cycle.
13. **P0 preemption is total.** When a new user-reported P0 arrives, stop or park unrelated lower-priority agents immediately, preserve their diffs, and rebuild the DAG around the P0. Do not keep the previous wave consuming shared context or files “in the background.”
14. **Review the diff, not the promise.** Review agents start only after the implementation owner stops editing and a concrete diff artifact exists. Interim architecture review may run in parallel, but it must label missing code as in-progress rather than a final defect.
15. **Keep independent file domains saturated.** A review, external wait, or long-running implementation blocks only nodes that consume its output or touch its owned files; it is never a global implementation barrier. After every spawn, completion, review finding, or user reprioritization, immediately rescan pending requirements by exact file ownership and dispatch every ready implementation, review, research, reproduction, or test-authoring lane whose files and frozen interfaces do not conflict. Do not leave runnable Todo items idle merely because another DAG branch is unfinished.
16. **Subagents author and check; the coordinator runs heavy verification once.** Subagents may implement, review, research, reproduce, inspect actual surfaces, write focused tests/E2E lanes, and run lightweight non-executing checks for their owned node (`cargo check` on the owning crate/target, TypeScript `tsc --noEmit`, syntax parsers, and `git diff --check`). They must not run `cargo build`, `cargo test`, test binaries, npm production builds, browser/E2E suites, project-wide test suites, or other commands that build or execute the product; those heavy commands are centralized after every participating writer stops. Each subagent reports the exact heavy verification commands. The coordinator then runs one narrow-to-broad unified pass under Testing rules 9–10.
17. **Pipeline review per node, not per wave.** Start an independent different-model review as soon as one implementation owner stops editing and its diff exists, while unrelated implementation continues. Select the reviewer by the node's risk and specialty; repository rules must never hard-code a specific model or provider for all reviews. Reviewers are read-only and do not run tests. Review findings reopen only that node and its dependents; all verification remains centralized under rule 16.
18. **Split or cancel silent workers early.** A worker that misses a requested progress checkpoint, spends repeated cycles re-reading or re-running commands without a concrete diff, or exceeds its file/contract boundary must stop and hand off current state. Reassign the smallest unfinished file slice; never let one silent node consume the orchestration critical path while independent work remains undispatched.
19. **tmux composer handoffs require a separate Enter.** Capture the target pane and confirm the composer is idle, send the complete message without `Enter`, then send `Enter` in a separate `tmux send-keys` call. Capture the target pane afterward and verify the submitted message appears; text left in the composer is not delivery.
20. **Prefer DeepSeek/GLM for implementation, GPT/Grok for review.** When available, delegate implementation and investigation tasks preferentially to DeepSeek or GLM, and reserve the main GPT agent and Grok primarily for review, integration, and correctness gates. This is a routing preference, not a lock: unavailable agents may be substituted, and a materially better-suited specialist may take any role; different-model review (rule 17) always remains mandatory and is never restricted to a fixed model.
21. **Check any subagent still running at 30 minutes.** At 30 minutes of wall time, the coordinator must inspect that worker: files actually edited, current step, remaining work, and whether it is looping. If it has a concrete next edit and is making progress, let it continue and re-check at the next 30-minute mark. If it has no diff, is re-reading the same files, missed a progress checkpoint, or cannot name the next edit, stop it immediately and either give a grounded next-edit instruction or reassign the smallest unfinished file slice. Do not wait past 30 minutes of aimless investigation.
22. **Record durable notes in MEMORY.md on the same cadence as rule 21.** When a subagent is checked at 30 minutes, finishes, or is stopped, write the concrete lesson (what failed, which file:line, what to do next) into `MEMORY.md` before starting the next wave. Skip one-off noise; keep tricks and invariants.



## Shared Worktree Safety

1. Treat every unowned staged or unstaged change as another contributor's work. Never overwrite, reformat, remove, stage, or commit it.
2. Never use broad staging or commit commands such as `git add .`, `git add -A`, or `git commit -a` in a shared worktree. Stage only explicitly owned paths and verify both `git diff` and `git diff --cached` before committing.
3. Before reporting or committing a completed task, run `git status --short`, `git diff --check`, and `git diff --cached --check`, then confirm the staged file set contains only the task's owned changes.
4. If the worktree does not compile, first identify whether the failure originates in owned files, another active agent's files, or the environment. Do not modify unowned files merely to obtain a green build, and report blocked verification with the exact failing command and source location.

## Error Handling

1. Fail fast with readable error messages carrying context (object id, parameters, key variables).
2. Never discard or mask errors through empty/broad `catch`, wildcard `Err(_)`, `.ok()`/`.ok()?`, success/`None` substitutions, dropped cleanup errors, or context-dropping conversion. Catching broadly is allowed only when every unexpected error is rethrown with context. Suppression is allowed only for one exact documented non-fatal category—including an explicitly matched superseded or cancelled background refresh—while preserving authoritative last-good state, with a negative test proving other errors propagate. `unwrap` outside tests and `expect("unreachable")` in production remain forbidden.
3. Never delete failure paths to "make tests pass." Either handle them or explicitly reject with an error.
4. Retry, rollback, and idempotency must be modeled explicitly — never rely on "it won't trigger accidentally" as correctness.

## Naming

1. Names must directly express intent: functions say what they do, variables say what they represent, types say what they model.
2. No `tmp` / `data` / `result` / `obj` / `foo` in long-lived or public interfaces.
3. Abbreviations only for universally understood terms (`ctx`, `id`, `cfg`, `db`, `tx`). Everything else spelled out.
4. Same concept, same name across the entire codebase.
5. Booleans prefixed with `is_` / `has_` / `should_` / `can_`.
6. **No task/spec numbering in code.** Do not embed task IDs (`TASK-01`), spec phase numbers (`Phase 3`), or step numbers (`Step 1/2/3`) into variable names, function names, file names, commit messages, or code comments. These belong in docs and tracking only.
7. **No dual-version names.** Do not name types, functions, files, flags, or comments `canonical`, `fallback`, `legacy`, `compat`, or `v1`/`v2` to mark a parallel implementation. Filesystem `canonicalize_*` path helpers are the only `canonical*` exception.

## Module Boundaries & Imports

1. Each module must clearly define exported types, functions, and responsibilities.
2. Import modules or explicit items instead of glob imports. Prefer `use mod_name;` when qualified access remains readable, and use grouped explicit imports when several items are needed.
3. Rust: public surface in `lib.rs` / `mod.rs`; implementation details `pub(crate)` or narrower.
4. No module A secretly importing module B's private helpers — boundary violations are forbidden.
5. Layering: `pi-cli` depends on `pi-coding` depends on `pi-agent` depends on `pi-ai`. Do not reach across layers into internals (e.g. pi-cli must not touch `pi_ai::providers` internals directly; go through `pi_coding`).

## Dependencies

1. Define dependencies at workspace level; reference with `workspace = true` (Rust) or root `package.json`.
2. Do not pin patch versions. Specify up to minor version only.
3. Every new dependency must state: what problem it solves, whether existing deps can suffice, maintenance activity, license, size, attack surface.
4. Do not add large dependencies for elegance. Every dep is future maintenance cost.
5. Dependency updates must be separate PRs, never piggy-backed on feature PRs.

## Testing

1. Tests in the same file as the code whenever possible (Rust: `#[cfg(test)] mod tests`).
2. Standalone `tests/` only for cross-module integration tests, framework requirements, or excessive contract/fuzz tests — and only with explicit justification.
3. Every test must fail on the exact plausible bug it claims to cover. When used as sole evidence for runtime or user-visible behavior, a source-text assertion, status-only check, or screenshot call is a false pass. Stale selectors, wrong or non-authoritative fixtures, unregistered lanes, timeouts long enough for a background poll to satisfy an "immediate" contract, comparisons between two equally-broken rows, and mocks that accept both old and new protocols are false passes and must be corrected before implementation approval.
4. Frontend tests must cover 100% of the user-reachable interactive surface: every place a user can input, select, view, or click must have executable test coverage (unit/DOM tests plus real-browser lanes for primary flows). A new or changed interactive control ships only together with its test; an untested control is incomplete work.
5. Coverage tools (e.g. `cargo llvm-cov`) supplement but do not replace test design. Coverage that executes tests MUST use `--release` (or `--profile release-dist`); never the default `dev`/`debug` profile.
6. Standard verification commands (toolchain 1.88.0; tests always `--release`):
   - `cargo +1.88.0 test -p pi-ai --lib --release`
   - `cargo +1.88.0 test -p pi-coding --lib --release`
   - `cargo +1.88.0 test -p pi-cli --lib --release`
   - `cargo +1.88.0 check --workspace`
   - `git diff --check`
   - E2E suites: `cargo +1.88.0 test -p pi-cli --test goal_loop_e2e --release`, `-p pi-coding --test workflow_full_e2e --release`, `--test trust_hook_wiring --release`, `--test handoff_prose --release`, `--test plugin_marketplace_e2e --release`, `--test rewind_checkpoint_snapcompact_e2e --release`, `-p pi-coding --test debug_tool_dap_e2e --release`, `--test sandbox_smoke --release`.
7. Environment-dependent tests (real browser, gdb, python3, PDF tools) must be skip-guarded when the tool is absent — but a BROKEN present tool (e.g. Chrome installed but too old for `--headless=new`) must fail with a clear message, not silently skip.
8. **No builds or test execution during implementation; lightweight checks are allowed.** Keep implementation and final verification as separate phases. Subagents may run only bounded, non-executing validation for their owned node: `cargo check` scoped to the owning crate/target, `tsc --noEmit`, syntax/parse checks, and `git diff --check`. They must not run `cargo build`, `cargo test`, test binaries, npm production builds, browser/E2E suites, project-wide checks, or any command that executes the product. Completing a subtask does not authorize heavy validation. After every production-code and intended-test authoring item is written and every writer has stopped, the coordinator runs the unified workspace check and the narrow-to-broad test/E2E pass under rules 9–10.
9. In the verification phase, verify from narrow to broad: the smallest targeted test, the owning crate suite, then workspace or E2E suites required by the change's blast radius. If verification exposes defects, collect and repair all known defects before compiling again; do not alternate between each individual edit and another build, and do not repeatedly run a known-blocked whole-workspace build.
10. Verification reports must distinguish failures caused by the current change, failures caused by concurrent unowned changes, and environment failures. Never claim a check passed unless that exact command completed successfully against the reported code state.

## Performance & Concurrency

1. Performance optimization comes after correctness, readability, and testing.
2. No "preventive" complex optimization without profile/benchmark/real input data.
3. Concurrency primitives must be modeled explicitly. No "looks thread-safe but relies on luck."
4. Avoid locks entirely unless no lock-free design exists. Locks are the recurring source of deadlocks (the identity-rekey cutover hang was one). Roughly 99% of scenarios do not need a lock — shared mutable state is a bad design; restructure with ownership transfer, immutable snapshots, message passing, command queues, or per-session isolation instead. If a lock is genuinely unavoidable, deadlock-freedom must be proven before landing: write down the full lock order, exercise the contended path with a dedicated test (gated spawner / forced interleaving), and re-audit every callsite that takes the lock during a multi-step operation.
5. Cross-thread shared state requires documented lock order, invariants, and visibility — otherwise do not introduce.
6. Every lock must have one stated protected invariant and the narrowest possible ownership scope. Never hold a synchronous lock across `.await`; an async lock across `.await` requires a documented reason and cancellation analysis.
7. Session runtimes, publications, event streams, command queues, and process ownership are session-local by default. A global registry may locate them but must not serialize independent sessions or become their mutable state owner.
8. Semaphores may bound a measured external resource but must not substitute for state isolation. New fixed concurrency or queue limits are product behavior and require an explicit requirement plus saturation tests.
9. Every performance optimization requires benchmark results (command + data + environment) in the PR, otherwise classified as style change.
10. **Release-only tests and debugging.** NEVER run `cargo test`, `cargo build`, `cargo run`, TUI/Web listeners, E2E, or gdb/lldb/DAP against the default `dev`/`debug` profile. Use `cargo +1.88.0 test ... --release` for crate/integration/E2E binaries, and `--profile release-dist` for deployed/production-shaped verification. Launch and attach only `target/release` or `target/release-dist` artifacts. Debug-profile observations (slowness, timeouts, flakiness, optimizer-sensitive behavior) are not evidence. `cargo check` remains allowed (type-check only, no execution).

## Security

1. Never write credentials, tokens, keys, cookies, session ids, or private endpoints into source, comments, logs, test fixtures, or docs.
2. Logs must never print full user input — only length, hash, or type summary.
3. Input boundaries (network, file, IPC, CLI) must be treated as potentially malicious — never "we're internal so no validation."
4. Filesystem operations must explicitly limit path scope — never concatenate user input into paths. Workspace containment: use the existing scoped-path helpers (`resolve_scoped_path` / `canonicalize_child_path`) and revalidate before writes (symlink-swap safe).
5. Any `unsafe` / `eval` / `exec` / reflection / dynamic loading must be listed separately in PR description with threat model.
6. Secrets at rest: auth.json and any credential-bearing settings files are written owner-only (0600); secret settings values are marked `Secret` in the settings catalog so views redact them; never round-trip redacted views back into the file.
7. Password/credential cryptography: never roll your own; use the existing `ring`-based primitives (PBKDF2 + AES-GCM with fresh salt/nonce).
8. External package sources (npm registry etc.): verify `dist.integrity`/shasum; reject HTTPS→HTTP downgrades.

## Logging & Observability

1. Strict log level separation: `error` = human intervention needed, `warn` = follow-up needed, `info` = important state change, `debug` = development information.
2. No `error` level for expected failures; no `info` level flooding control flow details.
3. Key state changes, external calls, and long task enter/exit must have logs with correlating ids.
4. Log structure must be machine-parseable (JSON or fixed `key=value`). No unescaped multiline content.
5. Critical paths must expose metrics and trace spans, not just logs.
6. Debug output from embedded engines (brush shell, kernels, adapters) must NEVER reach the user-visible transcript/status — route to debug logs or drop it.

## Anti-Patterns (Automatic Rejection Triggers)

1. **TODO/placeholder residue**: `TODO` / `FIXME` / `XXX` / `<placeholder>` / `tbd` / "待补" in code, comments, tests, or config.
2. **Swallowed or disguised errors**: reject empty catch, `unwrap` outside tests, `expect("unreachable")` in production, context-dropping conversion, and every branch that converts an unexpected failure into success/no-op/absence. Explicit forbidden examples from observed failures:
   - `Err(_) => Ok(None)` on cancel/load/rebuild paths: real I/O, factory, derivation, or shutdown failures become “nothing to cancel”.
   - `.ok()?` or `catch(() => {})` around user actions such as Download, open workspace path, cancel, remove, fork, or switch: the UI reports no outcome while the operation failed.
   - signaling completion before publication or rollback finishes, or signaling after rollback failure without removing the in-progress ticket: cancellation can spawn a zombie or permanently poison every retry.
   - making a component silent while the generic RPC layer has already emitted a toast: background refresh failures still leak globally.
   - dropping cleanup/rollback errors, or returning success after only part of the durable state was removed.
   Only one exact documented non-fatal category may be suppressed, and a negative test must prove every other error propagates.
3. **Dead code / commented code**: commented-out blocks, `if (false)`, unused functions — delete, don't comment.
4. **Deep nesting**: >3 levels of conditionals/loops without extraction.
5. **Giant functions/files**: single function >~60 lines without splitting; single file >~800 lines without splitting (language convention exceptions documented).
6. **Copy-paste**: same non-trivial logic ≥2 times with micro-variations (child-process lifecycle, Content-Length framing, redaction, session-identity computation all have shared helpers — use them).
7. **Style piggybacking**: large reformatting, renaming, or file moves in a feature PR making diff unreadable.
8. **Random dependency additions**: large library for a small utility; locked patch version; sneaking core dep upgrades in feature PRs.
9. **Import pollution**: `*` glob imports (when language allows), cross-private-API references, circular dependencies.
10. **Empty names**: `tmp` / `data` / `result` / `obj` / `foo` in long-lived or public interfaces; same concept with different names.
11. **Comment noise**: comments restating function names, signature-filling doc blocks, references to soon-expired task/PR numbers.
12. **Premature optimization**: complex optimization without profile/benchmark data; sacrificing readability for optimization.
13. **Undocumented unsafe/eval/exec/reflection/dynamic loading.**
14. **Implicit global state / implicit lock order / implicit cross-thread sharing** without documentation.
15. **Credentials/keys/tokens/private endpoints** in source, logs, test fixtures, or doc examples.
16. **Log level abuse**: `error` for expected failures; `info` flooded with control flow; critical paths without logging.
17. **False evidence and self-certification**: reject happy-path/status-only tests and every test/evidence pattern that cannot fail on the reported bug. Explicit forbidden examples from observed failures:
   - a test compares two rows that are both dirty and calls one the clean control;
   - a directory walker uses `entries.flatten()` or treats `read_dir` failure as an empty directory, then claims rollback left no artifacts;
   - an “immediate” UI test waits 30 seconds or allows an 8-second background poll to satisfy it;
   - a mock accepts both the obsolete and new protocol while claiming to prove the new protocol;
   - a browser lane records `pageerror`/`console.error` but never fails on them, or monitors desktop while leaving mobile unmonitored;
   - a screenshot call is unreachable because selectors are stale, or the screenshot file is named but not generated and inspected;
   - a standalone lane is not registered in the release runner, or a subagent reports assertion counts/test names without raw current-tree execution output;
   - a task reports `complete`, `green`, or “fixed” while any required check is `PENDING` or `FAILED`.
18. **Local paths / sensitive paths**: `/home/...` / `/mnt/...` / `~/.ssh` / `~/.aws` / internal hostnames / private IPs in source, comments, doc examples, commit messages, or test fixtures.
19. **Non-essential commenting.** Do not comment out code, config, or routes unless the user explicitly states the section must be disabled. Delete or leave active; if disabling is needed, remove the entry entirely rather than commenting it out.
20. **Canonical / fallback / legacy dual implementations**: a second path for the same contract (`canonical` vs other, scheme/protocol/parser/cwd/message `fallback`, `legacy`/`compat`/`v1` adapters, old+new kept together). Delete the superseded path; fail closed or retry the same typed contract. Automatic rejection under Core Engineering Principle 8.
21. **Debug-profile execution**: `cargo test` / `cargo build` / `cargo run` without `--release` or `--profile release-dist`; launching `target/debug/*`; attaching DAP/gdb/lldb to a debug-profile binary; using debug-profile timing, timeouts, or flakiness as evidence.

## Architecture

1. **Build incrementally.** Start with the smallest end-to-end working version, then add features on top of a stable, usable product. Do not replace a working product with premature complexity.
2. **Design for long-term evolution.** Do not adopt stopgap solutions that are expected to be replaced later. Every architectural decision should serve the system beyond the immediate problem.
3. **Study established solutions first.** Before designing from scratch, research how mature products solve the same class of problems. Prefer proven patterns and conventions over inventing new ones.
4. **Re-challenge the design, then stop.** At each design or review gate, challenge the current design again, compare viable approaches, and choose the simplest robust long-term design. Reject the nearest or easiest workaround, ugly special cases, patches expected to be replaced, and temporary parallel paths — including `canonical` / `fallback` / `legacy` dual implementations (Core Engineering Principle 8). Continue only while evidence reveals avoidable complexity or a stronger simpler option; stop when one design clearly satisfies current requirements and invariants with the fewest concepts.

These architecture principles apply when choosing among solutions required by the current problem; they do not authorize speculative future-proofing beyond Core Engineering Principle 8.

## Architecture Invariants

1. **Session isolation**: workflows, memory, and transcript state are scoped per session (`<repo-digest>/<session-id>` namespaces). A new session must never see another session's workflows. Session switches (`/new`, `/fresh`, `/switch`, fork) must rebind session-scoped storage.
2. **Fail-closed guards**: the session auth-resolver and the orchestration durable-recording/rebind guards are fail-closed and must never be weakened (no unauthenticated streams; no durable write without the binding).
3. **Trust never weakens**: extension/hook approval can upgrade `Ask`→`Trusted` at most; a stored denial can never be upgraded through any path. Host `pre_trust_decision` must run BEFORE project extensions are loaded/executed.
4. **Durable archives precede commits**: rewind/snapcompact archives are written + fsynced BEFORE the journal record that references them; never commit a compaction without its lossless sidecar.
5. **Bounded everything at the UI boundary**: transcript rows, panel activity feeds, IRC histories, tool-card outputs, overlay rows — all bounded and truncated to the rendering width; a long line must truncate, never overflow into single-character columns.
6. **No engine debug output in the UI**: brush/kernel/adapter stderr is captured for diagnostics only; user-visible surfaces render redacted, bounded content.
7. **Independent session execution**: each loaded session owns its application state, dispatcher, publication binding, event routing, and command execution. Switching the displayed session changes only the view target; it must neither stop nor rebind another session, and every multiplexed event must carry and preserve its session identity.
8. **Turn-stable model selection**: an active turn uses the provider and model captured when that turn started. A model selection made while streaming updates persistent session settings for the next turn only; model identity is the `(provider, model_id)` pair, never `model_id` alone.

## Frontend Design

1. **Consistent palette and style**: colors and component styling come from the shared theme tokens (`--accent`, `--bg`, `--border`, …); never introduce ad-hoc hex colors or one-off component styles. The overall palette must stay restrained — few colors, used consistently for the same meaning on every surface.
2. **Flat, square design**: the visual language is flat and rectangular (四四方方扁平化). No rounded corners unless a component explicitly calls for them; no decorative gradients or shadows.
3. **Mobile and desktop are first-class**: every layout change must be verified at desktop (≥1280px) and mobile (≤720px, 390×844) breakpoints. A design that only works at one breakpoint is incomplete.
4. **Alignment and fill**: sibling controls in a row must share consistent heights and lengths; rows must fill their container width (flex: 1 on the growing element, compact fixed sizes on buttons) — no dead space, no clipped controls, no elements of mismatched heights side by side. Use flexbox for row/column layout.
4a. **Isolated section layout (flexbox isolation)**: every rail/panel/section (Runners rail, Projects rail, terminal overlay, context pane) is its OWN flexbox column (`display:flex; flex-direction:column`); shared scrollable content lives on the `flex: 1 1 0; min-height: 0; overflow-y: auto` child inside it. Never lay out sibling sections on shared grid tracks — a track shared between two sections lets one section's content starve or squash the other (the runner-row 14px squash bug). Sibling sections stack as flex items with `min-height: 0` and each section caps its own growth (`max-height`/`flex-basis`) and scrolls internally; no section paints over or compresses another.
5. **Every clickable element has a hover style** (and a visible focus ring); a control without hover feedback is incomplete.
5a. **Buttons carry a suitable icon**: every primary action button pairs a compact icon (or glyph) with its label so users can distinguish and remember controls at a glance; an icon-less primary button is incomplete. Icons come from the shared glyph set (inline SVG or the established unicode set) and never replace the shared interrupt language.
6. **Shared interrupt language**: stopping in-flight generation (agent turn, hunk-review reply, or any other primary “work is running” control) is one language across TUI and Web: the verb **Stop**, the glyph **■**, `--error` tokens, and the same control height as that surface’s primary submit. Do not ship a parallel Abort/Cancel/warn-yellow sibling for the same action. Esc is the keyboard synonym of Stop. Wire/RPC identifiers may remain `abort`.
7. **Header and footer are never obscured**: overlays, drawers, menus, and panels must leave the header and the composer/footer visible and interactive. If content is tight for the space, redesign the layout — never squeeze, clip, or cover the chrome.
8. **Slow operations show progress**: anything that can take more than ~150ms shows an animation (skeleton, progress bar, spinner) from the moment it starts until it settles; fast operations must not flash (delayed-show gate).
9. **Data-not-ready placeholders and persistent cards**: when a metric, counter, or value has not loaded yet, render `--` (double dash) — never `0`, an empty string, or a stale/wrong number. A data card that cannot populate must still render its complete card structure and labels with `--` values (and a skeleton/animation while actively loading); never hide, omit, collapse, or remove the card because its request is pending, empty, or failed. The placeholder must be visually distinct from a real zero so the user can tell "not loaded" from "actually zero".
10. **Errors are centralized**: user-visible errors display in the ONE dedicated toast/error layer; components never scatter their own error surfaces, and showing an error must never shift or reflow the overall page layout.
11. **Screenshot gate**: every UI change is verified with inspected screenshots at both breakpoints on the real rendered surface before it can be marked complete (Core Engineering Principle 18 applies).

12. **Generous limits**: any user-facing cap (list rows, entries, search results, output sizes) must be set high enough that a normal heavy workflow never hits it — pick the value by measuring the slow path at the cap, not by default. When a user hits a cap in practice, raise it (10x is the default step) before considering pagination; a truncation notice that appears during normal use is a defect, not a feature.

13. **One verb per action**: a single user action has exactly one label across the product. Do not ship "Cancel" as a text button on one dialog and "×" on another for the same dismiss action — pick the quiet × close and use it everywhere. Do not ship "Reconnect" as a manual button when the system auto-reconnects — the user should never need to think about it.

14. **No duplicate surfaces**: the same information or control must not appear twice on one screen. If a tab already shows "Files", the pane inside it must not have another "Files" button. If a runner's status is visible as a colored dot, do not also print "Online" as text — the dot is the signal.

15. **Status is color, not prose**: connection and lifecycle states are communicated through color (status dot, border accent) and at most one compact glyph. Never render a full sentence like "Can't reach this runner" as the primary state indicator — use a muted/gray dot and let the user click for details.

16. **Cert trust is a click, not a wall**: when a self-signed certificate blocks a connection, the UI must offer an "Open to trust" link directly on the affected surface (runner card, runner header), not bury it in a dialog three clicks deep. The link opens the runner's own page in a new tab so the browser's native cert approval flow runs.

17. **Terminal previews are terminals**: tmux pane previews must use a monospace font stack that includes Nerd Font fallbacks, render ANSI SGR colors (not stripped plaintext), use a dark terminal background (not the panel background), clip overflow without scrollbars, and keep the font small enough to show meaningful content (≤11px). The whole preview card is the click target — no separate "Open terminal" button.

18. **Discovery lives in the rail**: LAN-discovered runners appear as rows in the Runners rail with a status dot and an Add button — never hidden inside a dialog behind a `+` button. The rail is the single place where all runners (added and discovered) are visible at a glance.


## Documentation Standards

1. Every spec, review, or research document must have verifiable facts backed by `<file>:<line>` references.
2. Do not use inference as evidence in review documents — verified or not verified, nothing in between.
3. Test plans must cover: unit tests, integration tests, negative tests, and regression tests.
4. Acceptance criteria must be executable checks with commands, not vague goals.
5. In research documents, mark inferred facts explicitly with "Inference:". List unchecked areas rather than implying coverage.
6. **Product inventory (`FEATURES.md`).** The same change that adds, removes, or changes a user-visible capability (CLI, TUI, Web, RPC, tool, hook, plugin, session, safety) must update repository-root `FEATURES.md`. Ground every claim in current source. Delete the superseded sentence in the same change; do not keep a parallel description. `FEATURES.md` is committed product inventory, not `TASKS.md` / `MEMORY.md` / `PLAN.md`.

## Git Commit Rules

1. Commit after each independent task; commit after each milestone, even if composed of multiple completed tasks.
2. A completed, focused-tested critical fix must receive the required different-model review and be committed as its own narrow change before starting or mixing in a new feature. Never strand a usable fix behind unfinished feature work; incomplete, failing, or unreviewed fixes remain uncommitted.
3. Commit messages must be concise, clear, and readable — directly stating what was completed.
4. No vague messages: `update`, `fix`, `wip`, `misc`, `changes`.
5. Do not mix unrelated changes in one commit.
6. Do not add `Co-authored-by`, `Signed-off-by`, or collaboration footers unless explicitly required.
7. Never run `git push`. This repository is local-only for the current engagement.

## Review Anti-Cheating Constraints

Every review must satisfy these or be rejected as incomplete:

### Prohibited Behaviors

1. **No "all pass" without findings** — LGTM with zero findings on unread changes is invalid.
2. **No skim-review** — must read every changed file and surrounding context.
3. **No style-only review** — cannot use style findings to cover correctness/security gaps.
4. **No impression-based feedback** — every claim needs evidence from source at the current commit.
5. **No inference as verification** — review documents do not accept `Inference`; unverifiable claims must be Open Questions.
6. **No unverified AI-generated findings** — each finding must be confirmed in source with valid line numbers.
7. **No "author will fix" deferral** — every P0/P1 finding must include actionable `Suggested Fix`.
8. **No batch formatting feedback** — mass "add newline/add comment/reorder" is not review.
9. **No file-without-line references** — every `<file>:<line>` must resolve at the reviewed commit.
10. **No placeholder deflection** — "may need more testing", "suggest further evaluation" is forbidden.
11. **No sensitive data in review documents** — absolute paths, credentials, internal IPs.
12. **No review document with TODOs** — all sections must be complete or marked `Not applicable`.

### Review Must Answer

1. Which repos/files/functions/types changed, and is each covered?
2. Which changes affect external consumers (API/on-chain/FFI), and was the superseded path removed rather than kept as `compat`/`legacy`/`fallback`?
3. Are state machines, protocols, encoding, events, error codes consistent?
4. Are failure paths, retries, duplicate input, and state conflicts handled?
5. Does test coverage prove "not fake pass"?
6. Are cross-repo/cross-service boundaries synchronized?
7. Any performance/resource/security regression?
8. Any new runtime assumptions (ports, env vars, directories)?
9. Any new "drift-prone defaults" (model names, ports, providers)?
10. Are docs/specs/comments synchronized with code?

## Writing & Output Standards

- Prefer concrete, verifiable language over abstract goals.
- Separate `In Scope` and `Out of Scope` in every spec.
- Define success as observable checks, not vague expectations.
- If a dependency changes, account for downstream repos and verification.
- If tmux/multi-agent collaboration is expected, define pane roles and monitoring.
- A finished spec must be executable, reviewable, and testable.
- When explaining code to users, default to concise pseudocode plus the core function names and parameter/variable names. Show real implementation excerpts or full data structures only when needed to resolve ambiguity, prove a claim, or enable a concrete action.
- Keep ordinary technical replies centered on the decision, observable behavior, material risk, and verification. Target one screen where possible; use compact bullets or tables and omit non-essential implementation detail without omitting blockers or required evidence.

## Model Change Cross-Review

Any change made by a model MUST be reviewed by a different model before landing, per the model hierarchy **gpt > glm >= grok > deepseek**. Code authored by a lower- or equal-tier model must be audited by a STRICTLY HIGHER-tier model (deepseek → grok/glm/gpt; glm or grok → gpt) with ZERO unresolved errors before it can merge — a review that finds blocking errors sends the work back to the author, not to another same-tier pass. This applies to:

- Code changes (production code, tests, config)
- Documentation updates
- Agent configuration modifications
- Dependency additions or updates

**Procedure:**
1. Model A completes the change and stages the work.
2. Model B (different model) reviews the staged changes — reads every diff line, checks correctness, security, and adherence to engineering principles.
3. Model B reports findings with `<file>:<line>` references.
4. If Model B approves, commit. If Model B rejects, Model A addresses findings and repeat.

**Never skip cross-model review on substantive changes.** Quick fixes, typo corrections, and single-line obvious fixes may be exempt, but when in doubt, review.
