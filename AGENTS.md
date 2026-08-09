# AGENTS.md — rpi (pi-rs)

Rules for AI agents working in this repository. Adapted from the OMP/pi
coding-agent conventions with rpi-specific structure, test commands, and
repository invariants.

## Eight Hard Constraints

Violating any of these anywhere results in immediate rejection — fix first, continue second.

1. **No TODOs or placeholders in main branch.** `TODO`, `<placeholder>`, `?`, `tbd`, "待补" — do not land.
2. **No inference as conclusion.** Every judgment must trace to `<file>:<line>` with executable verification. Never use "probably", "looks like", "probably should" as a conclusion.
3. **No local absolute paths or secrets in code, comments, docs, or test fixtures.** Only repo-relative paths (`<repo-root>/...`) or explicit placeholders (`<workspace>/...`). No `/home/...`, `/mnt/...`, `~/.ssh`, API keys, tokens, cookies, private endpoints, or internal IPs.
4. **Never push changes.** Do not run `git push` under any circumstances.
5. **Never auto-format.** Do not run formatters (e.g. `cargo fmt`, `prettier`, `black`, `gofmt`) or perform style-only reformatting unless explicitly requested.
6. **Never rollback with `git checkout --`.** Do not discard local changes via `git checkout --`, `git restore`, or equivalent. Rollbacks require manual review and explicit user action.
7. **Never print credentials, tokens, API keys, or environment variables.** Do not read, echo, cat, or output the contents of secret files (e.g. `.env`, `.env.*`, `*.pem`, `*.key`, `auth.json`, credentials files) or environment variables (`env`, `printenv`, `echo $VAR`). Never log or display sensitive values.
8. **Never access cloud service credentials in HOME.** Do not read, list, or access `~/.aws/`, `~/.gcloud/`, `~/.azure/`, `~/.config/gcloud/`, `~/.config/azure/`, `~/.kube/`, or any other cloud provider credential directories under the home directory.

## Repository Layout

- `crates/pi-ai` — model providers (anthropic/openai/responses/faux/imagegen), catalog, stream/retry/timeout.
- `crates/pi-agent` — agent loop: `Agent`, `AgentTool`, `AgentEvent`, tool-call reconciliation.
- `crates/pi-coding` — the coding agent: `Session` (turn execution), `Application` (state machine), tools (`tools.rs` + `tools/*`), orchestration (subagents, durable runtime), extensions (QuickJS + process), workflow (worktree/YAML DAG), storage (`session_store.rs`), settings, auth/trust/sandbox, memory, MCP, goal/loop.
- `crates/pi-cli` — the TUI (`tui.rs` + panels), REPL, JSON-RPC modes (`rpc.rs`, `acp.rs`), subcommands, themes, terminal images (kitty protocol).

## Core Engineering Principles

1. Prefer simple, readable, and low-error code over cleverness, micro-optimizations, or minimizing line count.
2. Keep control flow flat. Minimize `if-else`; prefer `match`/`switch`/early-return structures. Do not exceed 3 levels of nesting.
3. Return early on errors. For `Option`, prefer `ok_or` or `ok_or_else` to produce readable errors and return immediately. For `Result`, use `?` or explicit `match` then early return.
4. Prioritize reuse. Extract shared logic into functions, traits, or small utilities instead of copy-pasting. Abstract only when the same logic appears ≥2 times and will be referenced again.
5. Do not over-comment. Use clear names to express intent; add comments only for invariants, non-obvious "why", external spec references, or known bugs. Never comment what the code already says.
6. Do not work around problems by weakening requirements, silently dropping behavior, or simplifying tasks beyond what was requested.
7. **Single optimal version only.** Do not maintain multiple versions or backward compatibility. Always converge to one best implementation — breaking changes are acceptable. Long-term maintainability takes priority over preserving old interfaces.
8. **Solve only the current problem.** Do not design data structures, functions, abstractions, or interfaces for hypothetical future needs. Implement what is needed now with the simplest correct approach.

## Error Handling

1. Fail fast with readable error messages carrying context (object id, parameters, key variables).
2. Never swallow errors: empty `catch`, `unwrap` on production paths, `expect("unreachable")` in production, or error conversion dropping context are all forbidden.
3. Never delete failure paths to "make tests pass." Either handle them or explicitly reject with an error.
4. Retry, rollback, and idempotency must be modeled explicitly — never rely on "it won't trigger accidentally" as correctness.

## Naming

1. Names must directly express intent: functions say what they do, variables say what they represent, types say what they model.
2. No `tmp` / `data` / `result` / `obj` / `foo` in long-lived or public interfaces.
3. Abbreviations only for universally understood terms (`ctx`, `id`, `cfg`, `db`, `tx`). Everything else spelled out.
4. Same concept, same name across the entire codebase.
5. Booleans prefixed with `is_` / `has_` / `should_` / `can_`.

## Module Boundaries & Imports

1. Each module must clearly define exported types, functions, and responsibilities.
2. Prefer module-level imports (`use mod_name::*;`) over long per-item lists.
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
3. Every test must prove behavior, not just assert `true == true` or `status == 200`. A test must fail on a plausible bug (no fake passes: no happy-path-only, no tautological bounds).
4. Coverage tools (e.g. `cargo llvm-cov`) supplement but do not replace test design.
5. Standard verification commands (toolchain 1.88.0):
   - `cargo +1.88.0 test -p pi-ai --lib`
   - `cargo +1.88.0 test -p pi-coding --lib`
   - `cargo +1.88.0 test -p pi-cli --lib`
   - `cargo +1.88.0 check --workspace`
   - `git diff --check`
   - E2E suites: `cargo +1.88.0 test -p pi-cli --test goal_loop_e2e`, `-p pi-coding --test workflow_full_e2e`, `--test trust_hook_wiring`, `--test handoff_prose`, `--test plugin_marketplace_e2e`, `--test rewind_checkpoint_snapcompact_e2e`, `-p pi-coding --test debug_tool_dap_e2e`, `--test sandbox_smoke`.
6. Environment-dependent tests (real browser, gdb, python3, PDF tools) must be skip-guarded when the tool is absent — but a BROKEN present tool (e.g. Chrome installed but too old for `--headless=new`) must fail with a clear message, not silently skip.

## Performance & Concurrency

1. Performance optimization comes after correctness, readability, and testing.
2. No "preventive" complex optimization without profile/benchmark/real input data.
3. Concurrency primitives must be modeled explicitly. No "looks thread-safe but relies on luck."
4. Cross-thread shared state requires documented lock order, invariants, and visibility — otherwise do not introduce.
5. Every performance optimization requires benchmark results (command + data + environment) in the PR, otherwise classified as style change.
6. Default to release builds for performance or availability judgments — debug mode is too slow.

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
2. **Swallowed errors**: empty catch, `unwrap` outside tests, `expect("unreachable")` in production, error conversion dropping context.
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
17. **Fake test passes**: asserting `true == true` / `status == 200`, only happy path, mocking real deps without documentation.
18. **Local paths / sensitive paths**: `/home/...` / `/mnt/...` / `~/.ssh` / `~/.aws` / internal hostnames / private IPs in source, comments, doc examples, commit messages, or test fixtures.

## Architecture Invariants

1. **Session isolation**: workflows, memory, and transcript state are scoped per session (`<repo-digest>/<session-id>` namespaces). A new session must never see another session's workflows. Session switches (`/new`, `/fresh`, `/switch`, fork) must rebind session-scoped storage.
2. **Fail-closed guards**: the session auth-resolver and the orchestration durable-recording/rebind guards are fail-closed and must never be weakened (no fallback to unauthenticated streams; no durable write without the binding).
3. **Trust never weakens**: extension/hook approval can upgrade `Ask`→`Trusted` at most; a stored denial can never be upgraded through any path. Host `pre_trust_decision` must run BEFORE project extensions are loaded/executed.
4. **Durable archives precede commits**: rewind/snapcompact archives are written + fsynced BEFORE the journal record that references them; never commit a compaction without its lossless sidecar.
5. **Bounded everything at the UI boundary**: transcript rows, panel activity feeds, IRC histories, tool-card outputs, overlay rows — all bounded and truncated to the rendering width; a long line must truncate, never overflow into single-character columns.
6. **No engine debug output in the UI**: brush/kernel/adapter stderr is captured for diagnostics only; user-visible surfaces render redacted, bounded content.

## Documentation Standards

1. Every spec, review, or research document must have verifiable facts backed by `<file>:<line>` references.
2. Do not use inference as evidence in review documents — verified or not verified, nothing in between.
3. Test plans must cover: unit tests, integration tests, negative tests, and regression tests.
4. Acceptance criteria must be executable checks with commands, not vague goals.
5. In research documents, mark inferred facts explicitly with "Inference:". List unchecked areas rather than implying coverage.

## Git Commit Rules

1. Commit after each independent task; commit after each milestone, even if composed of multiple completed tasks.
2. Commit messages must be concise, clear, and readable — directly stating what was completed.
3. No vague messages: `update`, `fix`, `wip`, `misc`, `changes`.
4. Do not mix unrelated changes in one commit.
5. Do not add `Co-authored-by`, `Signed-off-by`, or collaboration footers unless explicitly required.
6. Never run `git push`. This repository is local-only for the current engagement.

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
2. Which changes affect external consumers (API/on-chain/FFI), and is compatibility maintained?
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
