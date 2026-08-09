# Known Issues

Known compatibility gaps, intentional divergences, and technical debt in `rpi`,
verified against upstream [`earendil-works/pi`](https://github.com/earendil-works/pi)
at `main@4c01c709380621c5ff2719162cd7a7973dcb2799`.

Severity legend:

- **HIGH** — breaks reading or interoperating with upstream data / clients.
- **MEDIUM** — silent behavior change or data loss on round-trip.
- **LOW** — cosmetic or edge-case divergence.
- **By design** — deliberate divergence, documented for reference.

## Session format

### RESOLVED — `UserMessage.content` accepts strings and arrays

rpi now accepts upstream plain-string user content and canonical content-block
arrays through `deserialize_user_content`, while continuing to serialize the
canonical array form (`crates/pi-ai/src/types.rs:154-162`). Compatibility tests
cover plain strings, arrays, invalid shapes, and canonical camelCase content
fields (`crates/pi-ai/src/types.rs:721-1123`).

### RESOLVED — Compaction/BranchSummary metadata round-trips

`SessionRecord::{Compaction,BranchSummary}` and the recorder paths preserve
optional `details`, `usage`, and `fromHook`
(`crates/pi-coding/src/session_store.rs:78-117,654-695`). The public wire type
and real recorder/reopen paths are covered by metadata round-trip tests
(`session_store.rs:2867-2963`).

### RESOLVED — `SessionRecord` is the public compatibility wire type

`SessionRecord` remains public and models the Pi v3 JSONL record variants
(`crates/pi-coding/src/session_store.rs:46-158`). Its serde shape is exercised
directly for label clearing, branch summaries, and compaction metadata
(`session_store.rs:2867-2902`), so it is no longer an untested dead enum.

### RESOLVED — label clearing omits the optional field

`record_label` inserts `label` only when a value exists
(`crates/pi-coding/src/session_store.rs:698-715`), and `SessionRecord::Label`
uses `skip_serializing_if = "Option::is_none"` (`session_store.rs:148-156`).
The raw JSONL and public-wire tests assert that a clear operation has no
`"label":null` field (`session_store.rs:2867-2902,3335-3362`).

### By design — rpi-only `todo_snapshot` entry type

rpi appends `"type":"todo_snapshot"` records to persist task/DAG state
(`crates/pi-coding/src/session_store.rs:798-800`); upstream's
`parseSessionEntries` skips unknown types, so Todo state does not survive a
round-trip through upstream. This is a deliberate rpi extension to the Pi v3
session format; rpi sessions restore todo state natively and the extra record
is inert to upstream parsers.

## Configuration and environment

### RESOLVED — legacy and canonical `trust.json` formats interoperate

rpi accepts both its versioned canonical envelope and upstream's flat
`{"<path>":true|false|null}` map, canonicalizes every key, migrates legacy
writes, and rejects malformed, conflicting, or future-version documents
fail-closed (`crates/pi-coding/src/trust.rs:203-223,327-418`). The compatibility
and corruption suite covers legacy decisions, null-as-Ask, aliases, migration,
unknown fields, and invalid versions (`trust.rs:458-823`).

### RESOLVED — session-directory environment and settings are honored

The resolved root follows `--session-dir > non-empty
PI_CODING_AGENT_SESSION_DIR > effective settings.sessionDir > default`
(`crates/pi-cli/src/session_run.rs:220-253`). The root is resolved once, stored
on the `Session`, and reused by startup resume/fork/import and later lifecycle
actions (`session_run.rs:398-404,566-580`; `crates/pi-coding/src/session.rs:1042-1049`).
`Settings.session_dir` is typed, validated as non-empty, and listed as a
supported effective setting (`crates/pi-coding/src/settings.rs:365-366,408-412,1508-1509`).
The binary compatibility regression covers create/resume/fork/new across every
precedence tier and explicitly trusts project settings before expecting them to
override the global root
(`crates/pi-cli/tests/session_rpc_cli_compat_e2e.rs:754-902`).

### RESOLVED — bash tool env vars compatible

Verified compatible: `PI_SESSION_ID`, `PI_SESSION_FILE`, `PI_PROVIDER`, `PI_MODEL`, and `PI_REASONING_LEVEL` exported to bash tool children match upstream; T94 additionally hardened every rpi-spawning test helper with `env_remove("PI_PROFILE")` so ambient env cannot corrupt suites.

The `PI_SESSION_ID`, `PI_SESSION_FILE`, `PI_PROVIDER`, `PI_MODEL`, and
`PI_REASONING_LEVEL` variables exported to bash tool children match upstream.

### By design — rpi-only variables

`PI_HOME`, `PI_FAUX_RESPONSE`, `PI_CACHE_RETENTION`, `PI_OAUTH_CALLBACK_HOST`,
`PI_SHARE_VIEWER_URL`, `PI_CLIPBOARD_IMAGE`,
`PI_SKIP_VERSION_CHECK` are rpi extensions. They do not affect compatibility.

### RESOLVED — pinned Rust toolchain

`rust-toolchain.toml` pins the stable channel to 1.88.0 (the MSRV); rustup
and CI both resolve to the pinned toolchain, and the file documents keeping
`.github/workflows/*.yml` in sync (`rust-toolchain.toml`).

## Resources (skills, prompts, themes, keybindings)

### RESOLVED — `.agents/skills/` roots are supported with trust gating

rpi discovers the platform user `.agents/skills/` root and trusted project or
ancestor `.agents/skills/` roots through typed resource roots. Candidate roots
are classified from their canonical targets: project-bound targets are excluded
when untrusted and forced to project scope when trusted, including global-path
symlink aliases. `.pi/skills/` retains the highest precedence
(`crates/pi-coding/src/resource_manager.rs`; focused resource-manager tests).

### By design — keybinding default chords differ

rpi defines its own default chord map in `default_bindings()`
(`crates/pi-cli/src/keybindings.rs:714-749`), so `ctrl+u` is `EditorClear` in
rpi while upstream maps it to `app.clear`. The dispatch layer uses stable
action IDs, and user config files override either set of defaults
(`keybindings.rs:800-870`). The divergence is deliberate: rpi owns its default
shortcuts and only binds actions that have an executable TUI handler.

### RESOLVED — later-loaded prompt templates win name conflicts

Prompt templates now use later-loaded-wins precedence: global, then project,
then explicit sources, with the winning template retaining its later position
(`crates/pi-coding/src/prompt_templates.rs:172-192`). Collision/order behavior
is locked by `later_duplicate_shadows_earlier_at_later_position`
(`prompt_templates.rs:428-444`).

### By design — skill `<location>` uses `skill://` URI

rpi's skill prompt lists locations as `skill://<name>` URIs
(`crates/pi-coding/src/resources.rs:1377`). The internal resolver maps
`skill://` URIs to the loaded skill's base directory
(`resources.rs:1368-1369`), keeping skill locations portable across machines
and sandbox-friendly. Upstream lists absolute file paths, but rpi deliberately
uses the URI scheme so prompts and extensions never depend on a local path.

### By design — rpi-only skill frontmatter fields

rpi additionally parses `globs`, `alwaysApply`, and `hide`/`hidden` from skill
frontmatter (`crates/pi-coding/src/resources.rs:660-673`). Upstream ignores
unknown frontmatter, so upstream-authored skills load unchanged. These fields
are deliberate rpi extensions for the skill selector and never alter upstream
skill behavior.

### By design — theme `extends` and strict unknown-field rejection

rpi themes support an `extends: "dark"|"light"` base
(`crates/pi-cli/src/theme.rs:521,555-560`) and reject unknown fields
(`theme.rs:503,514`). Upstream hardcodes two themes and is lenient, but valid
upstream theme JSON loads unchanged in rpi. The `extends` base and strict
validation are deliberate rpi features so theme files fail fast and cannot
carry silently ignored keys.

### By design — project-level keybindings file

rpi additionally loads `.pi/keybindings.json` (project-scoped) and applies strict
validation (`crates/pi-cli/src/keybindings.rs:812-870`); upstream only reads
`~/.pi/agent/keybindings.json`. The project-level file is a deliberate rpi
extension; upstream-authored global files remain compatible because rpi uses
the same upstream action→key shape.

### RESOLVED — read tool converts Office documents, EPUB, and Jupyter notebooks

rpi's `read` tool now routes PDFs and Office/notebook documents through
dedicated external converters. `doc_kind` maps `.docx/.xlsx/.pptx/.odt/.ods/
.odp/.rtf` to Office, `.epub` to EPUB, and `.ipynb` to Notebook
(`crates/pi-coding/src/tools/doc_convert.rs:50-77`). Office and EPUB text is
extracted via `pandoc -t plain`, with a LibreOffice `--headless --convert-to
txt` fallback for Office when pandoc is absent; notebooks use
`jupyter nbconvert --to script --stdout`
(`doc_convert.rs:109-186`). The branch shares the PDF converter's 30 s timeout
(60 s for LibreOffice cold-start), 32 MiB output cap, abort/cancellation
handling, and actionable missing-converter errors
(`doc_convert.rs:31,34,110-113,157-160,183-185`). `run_read` calls
`extract_doc_text` and renders the converted text with the same offset/limit
and oversized-line escape hatch as PDFs
(`crates/pi-coding/src/tools.rs:795-799`). Fixtures `sample.docx` and
`sample.ipynb` exercise the converters; integration tests verify DOCX and
notebook extraction, pagination, and the pandoc/LibreOffice fallback
(`tools.rs:3527-3575`; `doc_convert.rs:640-735`).

Peer contrast:

- **OMP** `read` still has broader URL binary fetch and embedded PDF-image
  handle extraction; rpi's converters cover the same Office/EPUB/notebook
  formats but do not yet expose line-range selectors on converted output or
  embedded-image handles.
- **grok-build** `read_file` handles text, PDF, PowerPoint, notebooks, and
  images through format-specific paths
  (`crates/codegen/xai-grok-tools/src/implementations/grok_build/read_file/mod.rs:45-120,420-535`).

### RESOLVED — web_search tool

rpi now registers a `web_search` tool that queries the DuckDuckGo Instant
Answer API. It is listed in the built-in tool identifiers
(`crates/pi-coding/src/tools.rs:113`) and wired into the tool factories
(`tools.rs:260,380,450`). The implementation respects truthy `PI_OFFLINE`
values (`1`/`true`/`yes`), caps `max_results` at 10 (default 5), applies a 15s
timeout, percent-encodes the query, and renders title/url/snippet blocks
(`crates/pi-coding/src/tools/web_search.rs:23-106,139-142,238-247`). Tests
cover offline parsing, URL encoding, response parsing, result formatting, and
tool registration (`web_search.rs:270-398`; `tools.rs:3611-3649`).

### By design — project instructions survive compaction outside message history

rpi stores project instructions in the agent's `system_prompt`, not as an
ordinary conversation message (`crates/pi-agent/src/agent.rs:30-33,750-753`).
Compaction replaces only the message vector
(`crates/pi-coding/src/session.rs:1847-1857,1920-1928`), so `AGENTS.md` and
other `<project_instructions>` assembled by `build_system_prompt` remain in
every subsequent provider request without re-injection or duplication. The
earlier warning incorrectly assumed the system prompt was part of compacted
history; no compatibility fix is required.

## Packages, import, export, and OAuth

### By design — `npm:` package source rejected

rpi's package model only supports `Local` and `Git` sources
(`crates/pi-coding/src/packages.rs:70`) and explicitly rejects `npm:` sources
(`packages.rs:629-630`). Configured `npm:` entries are listed for visibility
but never contribute resources (`packages.rs:843-846`, `:871-874`,
`:926-930`). This is a deliberate scope decision: rpi does not implement an
npm package manager or version-range resolver.

### By design — OAuth provider set differs

rpi supports the providers listed in `SUPPORTED_PROVIDERS`
(`crates/pi-coding/src/oauth.rs:17-22`): `anthropic`, `openai-codex`,
`google-gemini-cli`, `xai`, `openrouter`, and `kimi-coding`. Upstream supports
`github-copilot` and `radius`, which rpi does not implement; conversely, rpi's
`google-gemini-cli`, `openai-codex`, `xai`, and `kimi-coding` providers are
rpi-specific. Anthropic OAuth uses manual code paste (`oauth.rs:238-239`)
instead of an upstream callback server. This is a deliberate rpi credential
strategy: rpi targets the providers and flows it can maintain end-to-end.

### By design — HTML export rendering differs

rpi's HTML export is self-contained: it uses an inline-CSS/JS renderer
(`crates/pi-coding/src/export/mod.rs:19-28`) and the internal markdown-to-HTML
adapter (`export/markdown.rs:1-50`) with no external dependencies. Upstream
export uses `marked.js` + `highlight.js` and a custom TUI-to-ANSI-to-HTML
pipeline for tool entries. The divergence is deliberate: rpi prioritizes a
portable, dependency-free export that runs without a browser or CDN.

### By design — multi-format import is rpi-only

rpi imports 6 session formats (Pi, OMP, Codex, Claude, Grok, Droid)
(`crates/pi-coding/src/import/parsers.rs:55-63`); upstream `/import` accepts
only native Pi JSONL (`packages/coding-agent/src/modes/interactive/interactive-mode.ts:5546-5575`).
rpi is a superset here; upstream sessions import cleanly into rpi.

### By design — `sessionImportSources` gates automatic foreign resume

Foreign sessions are not silently imported on resume unless explicitly
enabled. The `sessionImportSources` setting lists allowed foreign source kinds;
native Pi is always implicitly included. Allowed values are `omp`, `codex`,
`claude`, `grok`, and `droid`; the default empty list means only native Pi is
eligible (`crates/pi-coding/src/settings.rs:415-416,511-519,1401-1403`).
Validation rejects unsupported or duplicate entries
(`settings.rs:1548-1560`). The effective list drives startup resume selection
(`crates/pi-cli/src/session_run.rs:404-405`) and the unified catalog adapter
(`crates/pi-cli/src/resume_catalog.rs:62-65`). The setting is reloadable and
exposed in the settings catalog as a typed string-list enum
(`crates/pi-coding/src/settings_catalog.rs:242`).

## Shell execution

### By design — system-bash spawn instead of embedded brush shell

rpi executes every bash tool call by spawning a system shell:
`/bin/bash` → `bash` on PATH → `sh` (`crates/pi-coding/src/tools.rs:1529-1537`),
then `Command::new(&shell).args(&shell_args).arg(command)` per invocation
(`tools.rs:1676-1678`). This is a deliberate portability choice: it keeps rpi
compatible with upstream `pi`'s shell behavior and avoids embedding a
platform-specific bash engine or maintaining in-process shell state. The
current system-bash path is fully functional; an embedded brush-style shell
remains a future feature, not a bug fix.

## Session behavior

### By design — resumed model is restored by rpi but not by upstream

On `--resume` with no `--model`, rpi restores the session's recorded provider
and model (`crates/pi-cli/src/session_run.rs:490-513`), following an explicit
precedence — explicit CLI model, authenticated resumed model, settings
default, then first authenticated model (`session_run.rs:478-479`). The same
precedence is documented for users (`docs/settings-trust.md:235-239`).
Upstream ignores the recorded model and always applies the settings/CLI
default (`packages/coding-agent/src/main.ts:420-482`). The divergence is
deliberate: rpi treats resume as picking up the recorded session, so the two
CLIs pick different models for the same resumed session.

### By design — resumed thinking level differs

rpi deliberately restores the recorded thinking level on resume as part of
its "pick up the recorded session" behavior. `resolve_initial_thinking_level`
uses the precedence CLI `--think` > model metadata > resumed branch (only
when the branch has a recorded thinking entry) > settings default > default
(`crates/pi-cli/src/session_run.rs:111-128`), and the result is applied at
`session_run.rs:543-557`. Upstream ignores the recorded level
(`packages/coding-agent/src/main.ts:469-472`), so the divergence is
intentional.

### RESOLVED — session lists use last-modified ordering

`list_sessions_in` orders files by descending filesystem mtime, then session
timestamp and path for a deterministic total order
(`crates/pi-coding/src/session_store.rs:1673-1710`). The regression fixture
asserts that a touched older session sorts ahead of a newer header timestamp
(`session_store.rs:3256-3290`).

### RESOLVED — extension trust hook

Trust decisions now emit a `trust_decision` extension event (allow-list 33→34, payload `{path, decision, isNew}`, approval-only wire so stored denials are never weakened — `crates/pi-coding/src/quickjs_host.rs`, `extensions.rs` `reduce_trust_decision`) and a blocking-capable `PreTrustDecision` host hook (`hooks.rs` `fire_trust_decision`, fail-open default, `failClosed` denies; `HookEvent::PreTrustDecision` in settings.rs + catalog). Composition: `resolve_project_trust_with_observation` + `apply_trust_hook_outcomes` (`trust.rs`), Application fires both at its trust (re)resolution point. Tests: never-weaken matrix, block/approve combos, allow-list + payload assertion, hook blocking/timeout/fail-closed, matcher filtering.

Upstream emits `emitProjectTrustEvent()` before consulting stored decisions
(`packages/coding-agent/src/core/project-trust.ts:60-100`), letting extensions
participate in trust decisions. rpi has no such hook
(`crates/pi-coding/src/trust.rs:180-200`).

### RESOLVED — fork labels are filtered and re-chained

Branch and cross-CWD fork construction exclude label entries from the retained
conversation, resolve the active labels, sort them deterministically, allocate
fresh ids, and chain them after retained content
(`crates/pi-coding/src/session_store.rs:1037-1112,1207-1244`). Tests cover
label clearing, latest-label selection, and fork rechaining
(`session_store.rs:2969-3012,3202-3362`).

### By design — `--continue` is deliberately native-only

rpi restricts `--continue` to native Pi sessions
(`crates/pi-cli/src/session_run.rs:410-414`); upstream picks any `.jsonl` in the
session directory. Because rpi imports multiple session formats, it must not
surprise-import a foreign session.

### RESOLVED — durable child recovery and mailbox-triggered revival

The orchestration runtime now recovers existing sidecar state before any write.
`bind_and_recover` (runtime.rs:1252) loads the prior `orchestration-state.json`
and prepares the recovered agent/job snapshots via
`prepare_parent_identity`/`prepare_parent_identity_binding` before committing
the durable binding; only when
no sidecar exists does it initialize fresh state
(`crates/pi-coding/src/orchestration/runtime.rs:1109-1129`). Rebinds are
serialized by the `durable_mutation` mutex and a `rebind_reserved` atomic
compare-exchange, so preparation and commit cannot race live mutations
(`runtime.rs:989-1043`).

Revival is atomic: `maybe_revive` holds `durable_mutation`, performs the
Parked→Queued claim, persists the claimed state, and only then launches the
revived child, making concurrent sends claim at most one revival job
(`runtime.rs:1221-1350`). Snapshot capture and the atomic file replacement both
run under the durable runtime's single ordering lock, and persistence errors
propagate without reporting false success (`runtime.rs:1138-1159`).

The public E2E suite in `crates/pi-coding/tests/durable_orchestration_e2e.rs`
exercises all target semantics across two-runtime restarts and runtime/
Application replacement: parked agents recover, unsettled jobs report
Cancelled, child JSONLs are reused, mailboxes restore, concurrent sends claim a
single revival, oversize writes reject before replacement, and sibling roster
visibility remains bounded. The file header lists the defended contracts
(`durable_orchestration_e2e.rs:1-35`).

### RESOLVED — subagent soft budgets and yield-driving

`JobSoftBudget` adds optional `max_requests`, `max_tokens`, and `yield_after`
to `OrchestrationConfig.soft_budget`
(`crates/pi-coding/src/orchestration/runtime.rs:134-159,180-184`). When a
configured limit is reached the child stops cleanly after the current turn,
settles as `Completed`, and both `TaskResult` and `JobSnapshot` carry
`soft_budget_exhausted: true` so the parent can decide whether to continue
the child (`crates/pi-coding/src/orchestration/jobs.rs:89-91,263-265,578-580`).
The runtime's `soft_budget_stop_hook` records the trigger and never fails the
job (`runtime.rs:2780-2783`). Tests in
`crates/pi-coding/tests/orchestration_jobs.rs:1379-1538` cover `yield_after`,
`max_requests`, `max_tokens`, accumulation across turns, and the unlimited
default.

### RESOLVED — sibling roster is injected; dedicated `ask` tool added

Subagents receive a bounded, XML-safe spawn-time `<peer_roster>` containing
Main and live siblings. Batch spawn registers every child first, snapshots
all rosters, then launches so same-batch siblings see each other
(`crates/pi-coding/src/orchestration/runtime.rs:1899-1988,2395-2445`). The roster
is capped at 64 entries / 16 KiB and keeps Main when truncating. rpi also now
ships a dedicated `ask` tool. `AskRuntime` owns the single-pending question
slot, publishes `SessionEvent::AskUser`, and resolves the awaiting tool call
when the frontend answers or cancels
(`crates/pi-coding/src/ask.rs:7-8,52-170`). The tool is listed in
`TOOL_NAMES` and wired into the standalone and default coding tool factories
(`crates/pi-coding/src/tools.rs:116,267,457,478`;
`crates/pi-coding/src/tools/ask.rs:19-22`). The TUI renders a pending ask as
`⟦ask⟧ <question> ⟦esc⟧` in the status line and routes answers/cancellations
(`crates/pi-cli/src/tui.rs:3112-3121,8719-8722`). Tests cover the round trip,
non-interactive rejection, timeout/cancel, id mismatches, and TUI rendering
(`crates/pi-coding/src/ask.rs:194-233`;
`crates/pi-coding/src/session.rs:5266-5460`;
`crates/pi-cli/src/tui.rs:22929-22972`).

### RESOLVED — subagent live progress in the TUI

Job cards now render a live progress one-liner — latest child IRC activity · wall-clock elapsed (e.g. `read tools.rs · 12s`), coarse `running · 12s`/`queued` fallback — from the `OrchestrationEvent` stream (`MessageDelivered`/`JobUpdated`/`AgentUpdated`), and the TodoDagPanel Subagent page adds a Progress line + timestamped activity log (64-entry bound, redacted, j/k scroll) + `history://` transcript hint (`crates/pi-cli/src/job_card_adapter.rs`, `todo_dag_panel.rs`, tui.rs render path). 6 new tests.

### RESOLVED — `hub list` includes agent type

The hub peer projection now carries and renders the registered agent type,
matching `hub jobs` and the TUI job cards
(`crates/pi-coding/src/orchestration/runtime.rs`;
`crates/pi-coding/src/orchestration/tools.rs`). Focused orchestration-tool tests
cover the peer roster output.

### RESOLVED — `/todo` has overview and detail DAG pages

`TodoDagPanel` lists the main DAG plus workflow DAGs, reports completed/open/
active/blocked counts, and opens a detail page with phases, dependencies,
execution state, and linked jobs (`crates/pi-cli/src/todo_dag_panel.rs:19-251`).
The TUI refreshes an open panel from canonical application state on Todo and
orchestration events. Viewport-aware prewrapping and scroll clamping share the
same display-row geometry, including narrow-terminal regressions.

### RESOLVED — settings panel section hierarchy

The settings panel now renders a two-level hierarchy — category tab → subsection (derived from the first dotted key-prefix segment, e.g. `retry.*` → Retry, prefix-less → General) → typed controls — with Enter drill-down, layered Esc, leaf cursor wrap, and a narrow-width (<72 col) flat fallback (`crates/pi-cli/src/tui.rs` `settings_subsections`/`settings_section_rows` + leaf rendering). Typed controls unchanged. 4 new tests.

The settings panel now derives native controls from `SettingValueType`:
boolean and enum controls, bounded numeric inputs, strings, lossless JSON
string arrays, generic lists/objects, and non-readable secret state
(`crates/pi-cli/src/settings_panel.rs:25-55,454-508`). Paste is routed to the
active settings editor before the hidden composer, invalid values stay open,
and Escape backs out one level at a time (`crates/pi-cli/src/tui.rs`).

### RESOLVED — composer chrome shows cost and task counts

The composer status line now appends session cost/usage totals (`$0.12 · 12.4k tok`, sub-cent precision so tiny costs never show `$0.00`) and a background-task count (`⟦N bg⟧`) when non-zero, projected from application state through the existing footer-refresh stats worker under the same identity guard — never computed in the render path, single-row and bounded (`crates/pi-cli/src/tui.rs` `composer_metadata`). 3 new + 2 extended tests (stale-result rejection, zero-hidden).

The composer header now renders the active model, compact thinking level, cwd,
git branch/dirty counts, context-window utilization, and bounded transient
status/activity in the top border (`crates/pi-cli/src/tui.rs:8558-8760`
`composer_border_lines`/`composer_header_line`). Git
metadata is collected off the render path by one bounded, generation-aware
background worker (`tui.rs:403-537`, footer-refresh request/admit/finish at
`tui.rs:4143-4233`). Each refresh discovers the
repository, copies its HEAD and index into a disposable Git directory with an
empty repo config, and points that sandbox at the real worktree
(`crates/pi-cli/src/code_review.rs:840-966`). Fixed plumbing commands then read
branch, tracked modifications, staged paths, and untracked paths without
executing repository-local `core.fsmonitor`, clean/process filters, textconv,
external diff, or rename drivers (`tui.rs:410-511`). Context utilization comes
from `session_stats()` in the same worker (`tui.rs:522-530`). Regressions cover
visible git/context segments, stale-result rejection, racily-clean hostile
fsmonitor and required clean/process filters, and unborn repositories with
staged paths (`tui.rs:18623-18990`).

## CLI surface

### RESOLVED — `--export` interface matches upstream top-level flag

`Cli` now exposes a top-level `--export <SESSION_PATH>` flag alongside
`--output`/`-o` and `--jsonl`, both gated by `requires = "export"`
(`crates/pi-cli/src/args.rs:28-39`). `run` dispatches the flag to the same
`export_session_command` used by the `export` subcommand, so the subcommand
and the flag share one implementation (`crates/pi-cli/src/lib.rs:103-105`).
Parser regression tests verify top-level `--export` parsing, mutual exclusion
with subcommands, and the `output`/`jsonl` requirements
(`crates/pi-cli/src/args.rs:866/884`). A library dispatch test confirms the
flag writes non-empty HTML (`crates/pi-cli/src/lib.rs:380).

### RESOLVED — `@file` expands text and image context safely

Prompt submission in TUI, JSON, initial prompts, and human event rendering all
uses `expand_prompt_in_workspace`. It supports quoted/escaped paths, text and
image attachments, additional workspace roots, size limits, XML escaping, and
canonical containment checks (`crates/pi-cli/src/file_args.rs:17-120,211-235`).
Missing files, parent escapes, absolute outside paths, and symlink escapes fail
closed; focused tests cover each contract (`file_args.rs:269-392`). This is a
deliberate security-strengthened implementation of upstream `@file` syntax.

### RESOLVED — `--resume` supports `-r`

`Cli::resume` declares `short = 'r'`, with the same selector conflicts as the
long option (`crates/pi-cli/src/args.rs:66-67`). The parser regression verifies
the alias and conflict behavior (`args.rs:708-715`).

### By design — no TUI flags

`--ui-mode` and `--alt` (alternate screen) are absent because rpi's TUI is a
normal-screen inline interface. Passing them to `rpi` is an unknown-flag error.

### By design — rpi-only flags and subcommands

`--cwd`/`-C`, `--add-dir`, and the `login`/`logout`/`models`/`sessions`/
`import-session`/`reload`/`export`/package-management/`config`/`update`/`llama`
subcommand families are rpi extensions (`crates/pi-cli/src/args.rs:222-333`).

### RESOLVED — `rpc` subcommand merged into the main binary

`rpi rpc` now runs the JSONL RPC control plane in-process: the `Command::Rpc`
variant forces RPC mode (≡ `--mode rpc`), and an explicit conflicting `--mode`
is rejected instead of silently overridden — the same authority the old
standalone `rpi-rpc` wrapper had
(`crates/pi-cli/src/args.rs:456-459,699-710`). The `rpi-rpc` binary is
deleted; `rpi` is the crate's only `[[bin]]` (`crates/pi-cli/Cargo.toml:13-15`).
Parser regressions cover subcommand parsing, redundant-but-consistent
`--mode rpc`, and conflict rejection (`args.rs:1012-1023`).

## RPC protocol

### RESOLVED — `get_commands` projects the primary command catalog

RPC discovery now returns the same ordered 22-command primary catalog used by
the interactive command UI, including `workflow`, `code-review` and `btw`, via
`visible_catalog()` (`crates/pi-cli/src/modes/rpc.rs:1570-1582`). Hidden
built-ins and dynamic prompt/skill/extension commands remain executable but do
not appear as primary discovery entries. The regression compares the complete
wire projection and order against `PRIMARY_COMMAND_NAMES`
(`rpc.rs:3324-3358`).

### RESOLVED — session-changing RPC commands report real cancellation

`new_session`, `switch_session`, `fork`, and `clone` return typed operation
outcomes and project their actual `cancelled` values; fork additionally returns
the selected text (`crates/pi-cli/src/modes/rpc.rs:1400-1402,1492-1504`). Tests
cover cancellation and successful false outcomes (`rpc.rs:2594-2649`).

### By design — response shapes are supersets

rpi's own JSONL RPC control plane extends upstream responses with extra fields (`get_state` adds `todoPhases`/`goal`/`runtimeSettings`, `get_tree` adds `activeLeafId`, `set_thinking_level` returns a richer `{requested,level,clamped,message}` payload — `crates/pi-cli/src/modes/rpc.rs`). All 32 upstream commands are implemented, plus rpi extensions; JSON consumers ignore unknown fields by convention, so lenient clients are unaffected. The superset is deliberate: the RPC mode (`rpi rpc` / `--mode rpc`) serves rpi's own frontends, which consume the richer shapes.

`get_state` adds `todoPhases`, `goal`, `runtimeSettings`; `get_tree` adds
`activeLeafId`; `set_thinking_level` returns a richer `{requested,level,
clamped,message}` payload. Strict-schema clients reject the extra fields;
lenient clients are unaffected. All 32 upstream commands are implemented, plus
40 rpi-only commands (`set_todos`, `loop_*`, `process_*`, `goal_*`,
`settings_*`, `workflow_*`).

### RESOLVED — `--listen` control plane with owned local UI and bounded pre-auth surface

Opt-in `--listen` starts a control plane around the same live `Application`
used by the text TUI/REPL. HTTP `POST /rpc` and WebSocket `/ws` share
`RpcDispatcher`, so commands target the on-screen session rather than a separate
RPC process (`crates/pi-cli/src/lib.rs`; `modes/listen.rs`).

Bearer-token auth, Origin rejection for tokenless browser requests, 4 MiB
payload/frame bounds, 16-command concurrency, and bounded outbound queues are
implemented. The pre-auth connection surface is capped at 64 tasks
(`crates/pi-cli/src/modes/listen.rs:19,137-153`). WebSocket application and
non-interactive UI subscriptions are established before the successful upgrade
can be observed, so post-handshake events cannot fall into a receiver-free
window (`listen.rs:482-487`). Remote clients cannot claim interactive extension
UI responses: both transports reject `ExtensionUiResponse`, leaving pending
interactions to the local TUI owner (`listen.rs:225-229,596-601`). The public
suite covers auth/origin, overload, two-client ownership isolation, flattened
non-interactive UI projection, and shutdown (`crates/pi-cli/tests/listen_control_plane.rs`).

Web `--listen` defaults to loopback binds (127.0.0.0/8 or ::1). Remote LAN
access requires both `--listen-allow-insecure-remote` and `--listen-token-file`
together with an explicit `--listen <lan-address>`; rpi prints a startup
warning that plaintext HTTP/WebSocket exposes the bearer token and control
traffic to passive network observers. `rpi agent serve` remains loopback-only
in this release. Neither surface provides TLS; use a TLS reverse proxy for
remote access. End-to-end TLS is tracked for a later release.

## Web client

### RESOLVED — `--listen` serves an embedded single-file web client with subprotocol token auth

Opt-in `--listen` now serves the web client itself: `GET /web` returns the
page and named asset routes serve hash-named bundles from an embedded asset
table (`crates/pi-cli/src/modes/listen.rs:208-211`). The client is a
vite + react + typescript project (`crates/pi-cli/web/`) whose production
build (`npm run build`, vite-plugin-singlefile) emits one self-contained
`dist/index.html` with JS+CSS inlined; `build.rs` turns `web/dist/` into a
`(path, mime, bytes)` table at compile time, so `cargo build` never requires
node (`crates/pi-cli/build.rs`). `RPI_WEB_DEV_DIR` overrides the embedded copy
for frontend iteration (`listen.rs:52-56`).

Browsers cannot set the `Authorization` header on WebSocket upgrades, so the
listener accepts the token as a subprotocol request
(`Sec-WebSocket-Protocol: rpi-auth.<token>`), compares constant-time, and
echoes the exact matched protocol per RFC 6455 (`listen.rs:58-63,192-198,
794-804`); without a configured token the subprotocol channel grants nothing
(the tokenless-loopback policy is unchanged — browsers are still rejected
because they always send Origin). Unit tests cover exact/wrong/empty/
whitespace/unrelated tokens, case sensitivity, prefix-match rejection, and
echo spelling (`listen.rs:1025-1093`).

The regression surface is the playwright E2E suite: 10 lanes
(`core goal xss abort reconnect switch mobile auth extras sessions`), each
driving the real `rpi --listen` binary over a real browser and asserting page
load, subprotocol connect, streaming, abort semantics, reconnect, model/thinking
switch, the panels, XSS safety, secret redaction, session management, and the
mobile viewport contract (`E2E.d/web/run.sh`; feature→lane matrix in
`E2E.d/web/README.md`).

### RESOLVED — web panels: todo/goal/workflow/session/settings/subagents/side-chat/maintenance

The web client ships eight panels plus a session sidebar, all backed by the
existing JSONL RPC dispatcher (`crates/pi-cli/web/src/panels/TodoPanel.tsx`,
`GoalPanel.tsx`, `WorkflowPanel.tsx`, `SessionPanel.tsx`, `SettingsPanel.tsx`,
`SubagentsPanel.tsx`, `SideChatPanel.tsx`, `MaintenancePanel.tsx`,
`SessionSidebar.tsx`). The RPC command catalog grew the panel-facing commands:
`session_list`, `todo_op`, `goal_pin`/`goal_unpin`/`goal_journal`,
`workflow_detail`, `task_spawn`/`job_list`/`job_cancel`/`hub_send`/
`job_output`, `side_chat_new`/`side_chat_switch`/`side_chat_close`/
`side_chat_prompt`/`side_chat_list`, `snapcompact`, `rewind`, `handoff`, and
`queue_list`/`queue_cancel` (`crates/pi-cli/src/modes/rpc.rs:710-781`). Each
is covered by a live-Application RPC test — e.g.
`todo_op_rpc_appends_completes_and_reopens_through_application`,
`workflow_detail_dispatches_panel_projection_through_rpc`,
`side_chat_rpc_round_trip_creates_prompts_switches_and_closes_tabs`,
`snapcompact_rpc_reports_a_to_b_tokens_without_provider`,
`rewind_rpc_lists_requires_one_target_and_rolls_back`,
`handoff_rpc_renders_envelope_without_provider_call`, and the D93 subagents
round trip `task_spawn_job_list_cancel_output_and_hub_round_trip`
(`rpc.rs:3982,4366,4757,4939,4971,5074,5291`).

### RESOLVED — web renderer: markdown tables/task-lists/images, strict mermaid, KaTeX

`markdown.ts` renders raw model text end-to-end: headings, tables (`md-table`),
task lists with checkbox glyphs, nested lists, blockquotes, fences, inline
code, links, and images (data-URI and safe-scheme only via `redact.ts`
`safeImage`/`safeUrl`) (`crates/pi-cli/web/src/markdown.ts:154-258,188-213`).
Mermaid fences hydrate asynchronously into SVG under
`securityLevel: 'strict'` (mermaid's XSS sanitizer is never disabled); parse
failures degrade to a styled raw-source block and never eval
(`markdown.ts:22-26,395-441`). KaTeX renders math via `katex.renderToString`
with `throwOnError:false`/`trust:false` after `redactSecrets`
(`markdown.ts:88-96`). Tool-execution cards render from
`tool_call`/`tool_execution_*` events, and thinking blocks collapse under a
`<details>` element (`markdown.ts:379-386`).

### RESOLVED — OMP-titanium web design and mobile-responsive layout

The web theme is the OMP titanium palette: `styles.css` documents the
provenance — the DARK/LIGHT constants are byte-for-byte OMP v17.2.6
`titanium.json`/`light.json` (test-locked by
`dark_palette_matches_installed_omp_titanium_exactly`), with semantic roles
mirroring the rpi TUI (`crates/pi-cli/web/src/styles.css:1-20`). A light
theme is opt-in via `data-theme="light"` on `<html>` (the same convention as
the exported session viewer). The layout is responsive: `100dvh` viewport
tracking keeps the composer above the on-screen keyboard, breakpoints at
≤768px (full-screen drawer overlays) and ≤480px (trimmed header, 44px touch
targets, hidden thinking select) cover tablets and phones, and
`prefers-reduced-motion` disables the pulse animation
(`styles.css:115-118,1641-1747`). The mobile contract is asserted end-to-end
by the `mobile` lane at 375×667 (`E2E.d/web/mobile_test.mjs`).

## Provider / model resolution

### RESOLVED — `:max` is a valid thinking-level suffix

`VALID_THINKING_LEVELS` includes `max`; both catalog and custom-id resolution
strip it from the model id and return the max thinking level
(`crates/pi-coding/src/resolve.rs:65-69,473-503`).

### RESOLVED — startup selects the first authenticated model when no default is specified

The production session path resolves explicit CLI model, resumed model,
configured provider/model, then the first authenticated catalog model
(`crates/pi-cli/src/session_run.rs:477-536`). The fixed
`DEFAULT_MODEL_SPEC` remains only in the lower-level source-compatible resolver
helper; it no longer determines no-flag startup behavior.



### RESOLVED — OpenAI Responses API stateful chaining

`previous_response_id` chaining is now opt-in (`SimpleStreamOptions.responses_stateful_chain`, `crates/pi-ai/src/types.rs:700-712`; settings key `responsesStatefulChain`): per-session chain state with 3-strike fallback to full history, delta-only input after the last assistant message, and chain reset on compaction/transcript replacement (`crates/pi-ai/src/providers/responses.rs:52-122`; pi-coding session.rs reset hooks). Covered by 6 tests (request building, chain advance, 3-strike fallback, stream-error break, reset, delta boundary).

rpi's `responses.rs` (3,641 lines) implements basic streaming for the OpenAI
Responses API (`crates/pi-ai/src/providers/responses.rs`), but it does **not**
use `previous_response_id` for stateful turn continuation. Every request sends
the full conversation history, identical to Chat Completions semantics. The
`session_id` field is only used as a prompt cache key
(`responses.rs:392-396`).

Peer contrast: OMP's `openai-responses.ts` (885 lines) implements
`buildOpenAIResponsesChainedParams` with `previous_response_id`, ZDR (Zero
Data Retention) detection, stale-chain fallback after 3 failures, and
WebSocket prewarm (`openai-responses.ts:256-303`). OMP's
`openai-codex-responses.ts` (3,549 lines) additionally supports WebSocket+SSE
dual transport with WS→SSE fallback and `x-codex-turn-state` sticky routing.

This is a feature gap, not a bug: rpi's primary target model is Claude
(Anthropic Messages API), not OpenAI. The Responses API chain support would
matter if rpi were to adopt OpenAI models as a first-class target.

### By design — `faux` API is rpi-only

`API_FAUX = "faux"` (`crates/pi-ai/src/types.rs:26`) is a test-only provider.
Upstream's `faux` provider uses `openai-completions` instead.

## Extension system

The extension model is an intentional security architecture with concrete
compatibility costs: executable code runs behind a versioned process protocol,
not inside the Rust host.

### By design — standalone extension manifest

Every extension must provide an explicit `pi-extension.json`. The manifest
selects the `process` or `quickjs` runtime and declares its capabilities; package
discovery refuses any extension resource that is not that explicit manifest
(`crates/pi-coding/src/extensions.rs:185-233,389-407`). The public extension
documentation describes this as the required package format
(`docs/extensions.md:1-47`).

**Compatibility impact:** extensions distributed only as inline
`package.json#pi.extensions` entries do not load unchanged. They need an rpi
manifest, and script extensions need an explicit `.js`/`.mjs` QuickJS `entry`.
This is deliberate validation and packaging metadata, not a missing loader feature.

### By design — child-process extension host

Both executable and QuickJS extensions run as child processes with piped JSONL
stdin/stdout. The host clears the inherited environment, validates the
executable or contained QuickJS entry, creates a separate process group, and uses
`kill_on_drop` for lifecycle cleanup
(`crates/pi-coding/src/extensions.rs:1435-1516`; `docs/security.md:169-205`).
QuickJS extensions run in-process (an embedded JavaScript engine) rather than
through an external runtime; the `process` runtime remains a separate
child-process host
(`docs/extensions.md:1-5`).

**Compatibility impact:** extensions cannot import host internals, retain direct
Rust `Application`/`Session` objects, patch host prototypes, or share the
in-process TUI renderer. Extensions written around those assumptions require a
protocol-backed port or a focused built-in feature. The process boundary is
intentional crash/lifecycle isolation; it is not an OS sandbox.

### By design — host-mediated UI instead of arbitrary in-process components

Process extensions cannot mount arbitrary Ratatui components or overlays.
Instead, they request typed host-owned operations: select/confirm/input/editor,
notifications and chrome, editor text, working indicators, themes, and tool
expansion state (`crates/pi-coding/src/extensions.rs:930-1032`). The QuickJS
runtime maps its supported `ctx.ui` methods onto those requests and explicitly
rejects component factories and terminal-input hooks
(`crates/pi-coding/src/quickjs_host.rs:392-393`). The TUI adapter owns
interactive requests and clears all state belonging to an invalidated extension
(`crates/pi-cli/src/extension_ui.rs:154-225,446-558`).

The important side-conversation use case is implemented as the built-in `/btw`
overlay rather than by weakening this boundary. It owns a detached agent,
independent transcript/editor/stream state, read-only default tools, optional
edit/exec tools, and `peek_main` (`crates/pi-cli/src/side_chat.rs:1-5,149-269`).
The public E2E coverage verifies main-session isolation, abort, read-only tool
capabilities, reopen persistence, and cleanup
(`crates/pi-cli/tests/side_chat_e2e.rs:468-581,817-918,1051-1151`).

**Compatibility impact:** third-party extensions that require custom Ratatui
components, `setEditorComponent`, autocomplete providers, or direct host-object
access still cannot be ported unchanged. They must use host-mediated UI or be
implemented as built-in Rust functionality.

### RESOLVED — multi-tab session container

The `/btw` side chat is now a multi-tab container (`SideChatTabs`): N named parallel sessions (max 8, names validated), `/btw new <name>` / `/btw <name>` / `/btw list` / `/btw close [<name>]`, instant switch (index move only, no fork/model call), background tabs keep streaming, and the tab bar renders in the side-chat panel (`crates/pi-cli/src/side_chat.rs:1105-1290`, `side_chat_panel.rs`). Legacy single-session behavior = the default `default` tab; sessions stay in-memory per the existing /btw isolation contract. 7 container + 1 command-surface + 2 e2e tests.

The saved-session selector in the CLI/TUI calls `Application::switch_session`,
whose cutover replaces the active session and scheduler state
(`crates/pi-cli/src/resume_catalog.rs:140-143,259-262`;
`crates/pi-coding/src/application.rs:1685-1705`;
`crates/pi-coding/src/loop_scheduler.rs:664-704`). The web client (`/web`) keeps
a single active session authoritative and restores it on reload; full
multi-session live switching is a CLI/TUI gap, while parallel side
conversations are covered by `/btw`.

### RESOLVED — `/live` realtime voice (configurable STT)

Hold-to-talk `/live` (default ctrl+space, configurable): mono 16 kHz i16 PCM capture (cpal behind the optional `live-capture` feature) → WAV in memory → POST to a USER-CONFIGURED OpenAI-compatible `{sttBaseUrl}/v1/audio/transcriptions` with `Authorization: Bearer {sttApiKey}` — no OpenAI hardcoding; transcript lands in the composer draft (never auto-submits). `Settings.live` (enabled/sttBaseUrl/sttApiKey[Secret]/sttModel/language/allowInsecure); TLS-only unless allowInsecure; 10s no-speech watchdog; bounded 30s backlog; mutual exclusion with the turn flow (`crates/pi-coding/src/live.rs`; `settings.rs` Live block; tui.rs LivePtt + status `⟦live⟧`/`Recording…`). 21 tests incl. mock-HTTP STT shape, TLS rejection, unconfigured errors, state machine.

### RESOLVED — terminal image rendering (Kitty protocol)

rpi has a terminal-image renderer wired into the TUI
(`crates/pi-cli/src/terminal_images.rs`, `crates/pi-cli/src/tui.rs`). The
Kitty protocol path now uses a spec-valid isolated support query with response
parsing that removes only the matching response and preserves unrelated bytes,
tmux detection is fail-closed (requires tmux >= 3.3 plus explicit
Kitty-capable outer-terminal evidence), PNG transmission is transmit-only
`a=t` with separate `s=<width>,v=<height>` controls and bounded 4,096-byte
base64 chunks, image and placement IDs are non-zero and randomized per
renderer, moved/disappeared placements receive ownership-scoped
`a=d,d=i,i=<image>,p=<placement>` deletion before replacement, and suspend/
yield/drop clean up only renderer-owned image IDs. Startup probing no longer
reads shared stdin, so it cannot swallow user keystrokes. iTerm2 and Sixel
fallbacks remain, and image bounds/MIME/memory guards are unchanged. Focused
regression coverage comprises `terminal_images::tests` (18 tests),
`transcript_image_plan_uses_inner_width_and_gutter_origin` (1 test), and the
`terminal_lifecycle` PTY clean-exit and startup input-preservation cases
(2 tests).

### By design — explicit capability declarations

The package manifest and the extension handshake both declare capabilities.
The runtime rejects requested capabilities not granted by the package manifest,
rejects registrations for undeclared or ungranted capabilities, and separately
gates each UI request
(`crates/pi-coding/src/extensions.rs:101-179,3242-3281,3523-3565`).

**Compatibility impact:** extensions written for a permissionless in-process
API must enumerate every command, tool, event, session-action, and UI capability
they use. An undeclared operation fails closed. This is the intended permission
model, not an unresolved compatibility bug.

### RESOLVED — extension-registered providers (QuickJS)

`pi.registerProvider({id, label?, api?, capabilities?, stream})` / `unregisterProvider(id)` on the QuickJS `pi` object (load-phase-only, `provider` capability-gated, identifier grammar, re-register replaces). Session-scoped `ExtensionProviderRegistry`; `commit_reload` syncs into the shared pi-ai registry so models with `api: <extension-provider-id>` route to the extension's async JS `stream(sessionId, messages, options)` — events translated to `AssistantMessageEvent` (start/thinking/text/tool_call/done/error), JS throws/errors surface as typed stream errors with secrets redacted; shutdown/unregister removes only the owning extension generation (`quickjs_host.rs`, `extensions.rs`, `pi-ai registry.rs`, `models_config.rs` auth exemption). 12 extension tests + conformance + pi-ai tests.

QuickJS (and formerly Bun) extensions cannot register or override LLM
providers: both methods throw `unavailable` and are listed in
`UNAVAILABLE_PI_METHODS`
(`crates/pi-coding/src/quickjs_host.rs:505-519,2118-2129`). This remains a real
API gap for provider extensions; the process protocol has no equivalent dynamic
provider implementation channel.

### RESOLVED — `ctx.ui` and session-control APIs for QuickJS extensions

QuickJS extensions now support the host-mediated rendering and session-control
surface. Phase 3 implements `ctx.ui.*`: interactive dialogs (`select`, `confirm`,
`input`, `editor`) and queued/query methods (`notify`, `setStatus`, `setWidget`,
`setTitle`, `setEditorText`, `pasteToEditor`, `setWorkingMessage`,
`setWorkingVisible`, `setWorkingIndicator`, `setHiddenThinkingLabel`,
`setToolsExpanded`, `getEditorText`, `getAllThemes`, `getTheme`, `setTheme`,
`getToolsExpanded`) issue `ExtensionFrame::Request { Ui, ... }` and settle promises
from host responses (`crates/pi-coding/src/quickjs_host.rs:271-393,1887-2205`).
Phase 4 implements session actions on `pi` (`sendMessage`, `sendUserMessage`,
`appendEntry`, `setLabel`, `setActiveTools`, `setThinkingLevel`, `setModel`) and
on `ctx` (`abort`, `compact`, `shutdown`, `waitForIdle`, `reload`), plus
per-invocation `ctx.signal` cancellation
(`quickjs_host.rs:443-518,1865-1880,2197-2205`). The `ui` and `session_actions`
capabilities are required synchronously and enforced host-side
(`quickjs_host.rs:1260-1265,1317-1321`).

A handful of extension APIs remain unavailable: `registerProvider`/
`unregisterProvider` (HIGH, above), `registerMessageRenderer`,
`registerEntryRenderer`, `exec`, `setEditorComponent`, and
`addAutocompleteProvider` are still rejected by the QuickJS bootstrap
(`quickjs_host.rs:392-393,505-519`). The `By design — host-mediated UI` section
covers why arbitrary Ratatui components and terminal-input hooks are not
exposed.

### By design — tool/command definitions are a subset

QuickJS extension tool registration rejects the advanced descriptor fields `constrainedSampling`, `renderShell`, `prepareArguments`, `renderCall`, and `renderResult`, and commands with `getArgumentCompletions` are unsupported (`crates/pi-coding/src/quickjs_host.rs:1946-1958`) — the same host-mediated architecture that rejects `registerMessageRenderer`/`setEditorComponent` (see `By design — host-mediated UI`): tool results and completion UIs are rendered by the Rust host, not by extension-supplied rendering callbacks, for rendering consistency and to keep extension code out of the terminal-input path. Simple tools/commands with a synchronous `execute` and standard JSON-schema parameters are fully supported.

QuickJS extension tool registration rejects the advanced descriptor fields
`constrainedSampling`, `renderShell`, `prepareArguments`, `renderCall`, and
`renderResult` (`crates/pi-coding/src/quickjs_host.rs:1946-1958`). Commands with
`getArgumentCompletions` are likewise unsupported. Simple tools and commands with
a synchronous `execute` function and standard JSON-schema parameters work; richer
rendering/completion contracts do not.

### RESOLVED — all upstream event-hook names are present

The QuickJS runtime allow-lists all 33 supported extension event names through
`SUPPORTED_EVENTS` and registers them through the `event_hooks` capability
(`crates/pi-coding/src/quickjs_host.rs:169-180`). Simple event, tool, command,
and shortcut extensions can therefore be ported after adding the required
manifest and declarations, provided they do not depend on one of the
unsupported APIs above.

## Sandbox and process isolation

rpi ships an **opt-in Linux filesystem sandbox** (`Settings.sandbox`) for the
`bash` tool, process extensions, and subagent children. It is default-off,
same-user confinement (not privilege isolation), and applies per command or
child spawn. QuickJS in-process extensions remain same-process. The supervised
process manager that hosts long-running `/process` commands is outside the
per-call bash sandbox.

### RESOLVED — opt-in Linux filesystem sandbox for bash

rpi now ships an opt-in filesystem sandbox for the `bash` tool. `Settings.sandbox`
configures fresh Linux mount/pid/net namespaces via `unshare`, with a tmpfs root,
`pivot_root` host-root detachment, bind-mounted allowed paths, read-only system
binds, and denied-path overlays. Network is off by default
(`crates/pi-coding/src/sandbox.rs:1-83,36-62,106-153,270-363`). The per-call
`bash` parameter `sandboxed` overrides the setting for one command, defaulting
to cwd + agent dir when no settings block exists
(`crates/pi-coding/src/tools.rs:1141-1179`). Validation fails closed on
non-Linux platforms, missing `unshare`, and a cwd outside allowed paths
(`sandbox.rs:62-105`). Integration smoke tests exercise denied `/etc/passwd`,
cwd write access, default network isolation, allowed/denied path overlays,
timeout kill of the namespace tree, and per-call override
(`crates/pi-coding/tests/sandbox_smoke.rs:23-245`). Unit tests in `sandbox.rs`
cover config resolution, wrapper argv shape, env serialization, and validation
(`sandbox.rs:368-617`).

`read`, `write`, and `edit` remain unsandboxed (they use path-level permission
rules instead). The sandbox is same-user confinement, not privilege isolation.
OS isolation for process extensions and subagent children is implemented when
their respective settings enable the sandbox; an always-on, privilege-reducing
OS boundary remains future work.

### RESOLVED — path-level permission rules

`Settings` exposes typed `permission_rules: Vec<PermissionRule>` where each
rule carries an `action` (`allow`|`ask`|`deny`), a `path` prefix (longest match
wins; a trailing `*` is accepted; relative paths resolve from the session
cwd), and an optional `tools` allowlist
(`read`|`write`|`edit`|`glob`|`grep`)
(`crates/pi-coding/src/settings.rs:506-526,640-650,970-1033`). The rule engine
runs before the capability-wide approval mode: `deny` blocks outright,
`ask` forces interactive confirmation even when the mode would allow, and
`allow` bypasses the capability ask; precedence is `deny > ask > allow`, with
the longest prefix breaking ties
(`crates/pi-cli/src/approval.rs:16-59`). Bash and other non-path tools are
not addressable and fall through. The setting is exposed in the catalog as
`permissionRules` under `TrustSecurity` with `RELOAD` apply behavior
(`crates/pi-coding/src/settings_catalog.rs:309`). Tests cover rule parsing,
validation, manager round-trip, precedence, live per-tool-turn
`deny`/`ask`/`allow` behavior, and relative-path resolution
(`crates/pi-coding/src/settings.rs:2978-3305`;
`crates/pi-cli/src/approval.rs:470-589`;
`crates/pi-cli/tests/approval_e2e.rs:677-760`).

### RESOLVED — parent-process hardening disables core dumps and ptrace attach

`main` calls `pi_cli::harden_process()` before argument parsing, dispatch, or
the panic hook so no sensitive path runs in an unhardened state
(`crates/pi-cli/src/main.rs:17`). The helper is cfg-guarded and best-effort:
Linux uses `nix::sys::prctl::set_dumpable(false)` (`PR_SET_DUMPABLE=0`) to block
ptrace attach and `/proc/<pid>/mem` access, and unix platforms set
`RLIMIT_CORE=0` via `setrlimit` so crashes cannot write core dumps; every
syscall result is ignored so unsupported platforms start normally
(`crates/pi-cli/src/lib.rs:77-94`). The `harden_process_runs_without_panic`
unit test exercises the startup path on unix
(`crates/pi-cli/src/lib.rs:369).

### RESOLVED — OS isolation for process extensions and subagent children

The filesystem sandbox is generalized (`SandboxConfig::wrapper_command` + `run_in_sandbox`/`spawn_piped`, killpg on timeout/abort): process extensions spawn through the sandbox when `settings.sandbox.enabled` (extension cwd + agent dir allowed; fail-closed validation), and subagent children run confined when `orchestration.sandboxed` (default false; allowed = workspace + agent dir + sandbox.allowedPaths; resolver threads into every child tool incl. bash). QuickJS in-process extensions remain same-process (documented; trust boundary = trust/approval path). `sandbox.rs`, `extensions.rs` process-spawn region, `runtime.rs` child factory, `session_run_blueprint.rs` wiring. Tests: sandboxed process extension cannot read /etc/passwd; sandboxed subagent child confined to workspace; flag-off keeps current behavior.

Remaining limits: the sandbox is opt-in and default-off, confines filesystem
access only (no network/syscall/credential isolation), and is same-user
confinement. QuickJS in-process extensions rely on the trust/approval path.
Workflow git worktrees isolate branches and working files, not host
privileges (`crates/pi-coding/src/workflow_worktree/mod.rs:1-83`). A stronger,
always-on OS boundary remains future work.

### By design — project trust gates executable resources, not tool authority

Project trust is a fail-closed load decision for project-local extensions,
skills, prompts, themes, keybindings, packages, and settings
(`docs/security.md:12-60`; `crates/pi-coding/src/trust.rs:17-40`). Project
extensions are launched only from trusted manifests
(`docs/extensions.md:86-97,264-287`). Tool approval is intentionally separate
from that trust decision.

This is deliberate: trusting a project authorizes its local resources to load;
it does not claim to sandbox those resources or commands. The HIGH sandbox,
path-rule, and process-hardening gaps above therefore remain visible even
though the trust model itself is working as designed.

### RESOLVED — users can attach directly to a running PTY

The `/ps` panel attaches only to a live PTY process. Printable input,
Enter/navigation/control keys, and paste are routed directly to
`ProcessManager`; `Esc`, `Ctrl+]`, and the legacy `Ctrl+5` encoding detach
locally, while process exit or I/O failure auto-detaches
(`crates/pi-cli/src/tui.rs:5716-5809,5880-5884,6201-6231`).

The attachment is a dedicated overlay, so input bypasses the model tool call,
composer, and session transcript. Unit coverage verifies direct routing,
detach precedence, bounded output, and no transcript entry on process exit
(`crates/pi-cli/src/tui.rs:11205-11500`); the real PTY integration exercises
attach, typing, child output, process-exit auto-detach, and `Ctrl+]` detach
(`crates/pi-cli/tests/process_ps_pty.rs:436-525`).

## Missing tools (OMP/grok have, rpi does not)

rpi ships a lean core tool set: `read, bash, edit, write, grep, find, glob, ls,
todo, process, task, hub, goal`. OMP documents 31 tools; grok registers native
tools plus compatibility ports. The following are missing entirely from rpi
(verified by grep across `crates/`).

### RESOLVED — `browser` tool for headless web automation

rpi now registers a `browser` tool that drives a headless Chrome/Chromium
instance over a hand-rolled Chrome DevTools Protocol (CDP) client using
`tokio-tungstenite`. Supported actions are `navigate`, `click`, `fill`,
`screenshot`, `extract`, `list_tabs`, and `close`
(`crates/pi-coding/src/tools/browser.rs:1-83,79-1095`). Chrome discovery
prefers `CHROME_PATH`, well-known install locations, and then `PATH`; a missing
binary is rejected with an actionable message (`browser.rs:50-59`). The tool
is listed in `TOOL_NAMES` and wired into the coding tool factories
(`crates/pi-coding/src/tools.rs:132,285,490,516,547`). Tests cover action
parsing, URL validation, argument shape, screenshot path resolution, missing
Chrome rejection, and a skip-guarded real-browser smoke against a local `data:`
page (`crates/pi-coding/src/tools/browser.rs:867-975`).

The native desktop `computer` automation tool (capture/input) remains
unimplemented.

### RESOLVED — image generation subsystem

Full image generation is implemented: a pi-ai OpenAI-compatible `{base}/images/generations` client (`crates/pi-ai/src/providers/imagegen.rs`, auth from model resolution with `images.genApiKey`/env fallback — nothing vendor-hardcoded), `ApiProvider::generate_image` + `Model.image_generation` capability, and the previously-N/A `api=openrouter-images`/`imagegen` providers now route here. The `generate_image` tool resolves model/key/base (model arg > `images.genModel` > active model; `images.genBaseUrl` override), bounds prompt (4 KiB)/n (1..4)/size whitelist, pre-checks decode (128 MiB) and dimensions (16 MP) before saving under workspace containment, and returns paths only (`crates/pi-coding/src/tools/image_gen.rs`). Debug impls redact keys; no image bytes in transcripts. Mock-server tests cover request shape, saving, bounds, capability gate, overrides, redaction.

`inspect_image` is implemented: bounded deterministic report (format/dimensions/color type/file size/EXIF orientation for JPEG/WebP/brightness mean+stddev/8-bin dominant-color histogram), 32 MiB file gate + 16 MP decode pre-check + 128 MiB alloc limits + 8M-pixel stats thumbnail, workspace path containment, actionable errors (`crates/pi-coding/src/tools/image.rs`, reuses the workspace `image` crate).

OMP ships `generate_image` (6 providers: OpenAI/Antigravity/OpenRouter/xAI/
Gemini — `omp://tools/generate_image.md`) and `inspect_image` (vision-model
analysis of a local image, auto-resize, WebP exclusion —
`omp://tools/inspect_image.md`). Grok ships image generation/editing and
video generation tools with byte/dimension/storage budgets
(`analysis.md:1681-1683`, feature gates `image_gen`/`video_gen` at `:819`).
Video generation remains unimplemented; image editing and vision analysis
beyond the deterministic inspect report are not covered.

### RESOLVED — AST tools (ast_grep and ast_edit)

rpi now registers `ast_grep` and `ast_edit` as built-in tools, backed by the
`ast-grep-core`/`ast-grep-language` crates. `ast_grep` supports tree-sitter
structural search with `$NAME`/`$_`/`$$$NAME` metavariables, a 50-match
limit, and 15 enabled grammars
(`crates/pi-coding/src/tools/ast_grep.rs:32-53,61-97,108-198`).
`ast_edit` applies pattern→rewrite replacements, validates patterns before
touching files, serializes writes through the shared per-file mutation queue,
and bounds the inline diff
(`crates/pi-coding/src/tools/ast_edit.rs:5-6,32-53,64-103,107-148,190-203`).
Both tools are listed in `TOOL_NAMES` and wired into the read-only and default
coding tool factories (`crates/pi-coding/src/tools.rs:113,260-262,310-311,
380-382,450-452`). Tests cover language gating, pattern parsing, metavariable
matching, rewrites, out-of-scope paths, and concurrent-edit serialization
(`ast_grep.rs:292-390`; `ast_edit.rs:244-367`).

### RESOLVED — `lsp` code-intelligence tool

rpi now registers an `lsp` tool that spawns per-language servers over stdio
JSON-RPC (Content-Length framing via `crates/pi-coding/src/tools/lsp_client.rs`).
Implemented actions are `hover`, `definition`, `references`, `diagnostics`,
`symbols`, `rename`, `code_actions`, `capabilities`, `status`, and `reload`
(`crates/pi-coding/src/tools/lsp.rs:1-83,36-44,211-1196`). Language detection
maps `rust` → `rust-analyzer`, `typescript`/`javascript` →
`typescript-language-server --stdio`, `go` → `gopls`, and `python` →
`pyright-langserver --stdio` (`lsp.rs:50-97`). `rename` applies workspace
edits through the shared mutation queue; unsupported resource operations are
rejected (`lsp.rs:149-209,510-609`). The tool is listed in `TOOL_NAMES` and
wired into the coding factories
(`crates/pi-coding/src/tools.rs:132,290,494,520,551`). Tests cover action
validation, language detection, UTF-16 offsets, diagnostics/symbols/code-action
formatting, workspace-edit application, a fake LSP server harness, and a
skip-guarded `rust-analyzer` end-to-end smoke
(`crates/pi-coding/src/tools/lsp.rs:1204-1582`;
`crates/pi-coding/src/tools/lsp_client.rs:408-680`).

A persistent codebase-wide index (grok's `xai-codebase-graph`) is not
implemented; per-call LSP covers the immediate symbol-navigation and rename
surface.

### RESOLVED — `github` tool

rpi now registers a `github` tool that queries the GitHub API. It prefers the
`gh` CLI (argv-built requests, no shell interpolation, token never handled by
rpi), with a `GH_TOKEN`/reqwest fallback when `gh` is absent
(`crates/pi-coding/src/tools/github.rs:1-83,87-112,478-604`). Supported actions
are `search_issues`, `get_issue`, `list_issues`, `create_issue`,
`comment_issue`, `list_prs`, `get_pr`, `list_commits`, `view_file`, and
`search_code` (`github.rs:25-34,86-88`). Results and error hints are passed
through `redact_secrets` so tokens cannot leak into tool output
(`github.rs:837-845`). The tool is listed in `TOOL_NAMES` and wired into the
coding factories (`crates/pi-coding/src/tools.rs:132,291,495,521,552`). Tests
cover schema/action enumeration, request construction, argv rendering, missing
binary handling, output rendering for issues/PRs/commits/code/files, secret
redaction, and three skip-guarded real `gh` smokes
(`crates/pi-coding/src/tools/github.rs:848-1368`).

### RESOLVED — eval and notebook tools

`eval` runs persistent kernels with cross-cell state: Python (`python3 -u -I` embedded driver, length-prefixed JSON frames, 64 KiB per-stream truncation, 30s cell timeout with killpg + lazy respawn) and JS (embedded rquickjs on a dedicated thread with memory/stack caps + interrupt deadline — no node dependency). `notebook` reads (8 MiB gate, cell listing with bounded previews), executes (code cells through the Python kernel, continues after runtime errors, `--write` persists outputs + execution_count), and edits (appends markdown/code/raw cells preserving unknown top-level fields) .ipynb files (`crates/pi-coding/src/tools/eval.rs`, `tools/notebook.rs`). Errors classified syntax/runtime/timeout; missing `python3` actionable. 29+ tests (plus 57 wave-14 qa additions across eval/notebook/imagegen).

A session-scoped `debug` tool (DAP client over stdio, Content-Length framing shared via `tools/framing.rs`): launch (gdb/lldb-dap/debugpy, PATH-validated), set_breakpoint, continue_/pause/step, stack_trace/variables/evaluate/threads, terminate with killpg + bounded grace; one adapter per session, redacted stderr, bounded event queue/output (`crates/pi-coding/src/tools/debug.rs`).

OMP ships `debug` (DAP sessions: launch/attach, breakpoints, evaluate,
stack traces, memory read/write — `omp://tools/debug.md`), `eval` (persistent
Python/JS cell runtime with cross-cell state — `omp://tools/eval.md`),
`checkpoint`/`rewind` (mark state, collapse exploratory context —
`omp://tools/checkpoint.md`), and `security_scan` (software security review
pipeline — `omp://tools/security_scan.md`). Security scanning remains
unimplemented.

### RESOLVED — per-project `memory` tool

rpi now registers a `memory` tool that persists note-style entries per project
under `<agent_dir>/memory/<repo-digest>/entries.jsonl`. The repository digest
is the first 32 hex chars of the canonical cwd SHA-256, so memory is
project-scoped and path-safe (`crates/pi-coding/src/memory.rs:1-83,68-88`).
Actions are `learn`, `recall`, `list`, and `forget`; bounds are 1 MiB per entry
and 100 entries per namespace (`memory.rs:69-72,134-213`). Writes are
serialized and atomic, and obvious credential shapes are redacted at the store
boundary (`memory.rs:86-88,327-349`). The tool is listed in `TOOL_NAMES` and
wired into the coding factories
(`crates/pi-coding/src/tools.rs:132,292,346,363,394,408,496,522,553`). Tests
cover learn/recall/forget/list round-trips, ranking, persistence, size/count
bounds, namespace derivation, secret redaction, source-session provenance, and
action validation (`crates/pi-coding/src/memory.rs:572-877`).

External memory backends are now supported via `Settings.memory.backend`
(`off|local|hindsight`): `local` keeps this built-in note store; `hindsight`
swaps the memory tool for a `recall`/`retain`/`reflect` trio that calls a
configured Hindsight HTTP API (`POST /v1/default/banks/{bank}/memories`,
`/recall`, and `/reflect`; `PUT /v1/default/banks/{bank}` to ensure the bank
with optional missions). Configuration is explicit: `memory.hindsightApiUrl`
(required), optional `memory.hindsightApiToken` bearer token (secret-redacted),
`memory.hindsightAllowInsecure` to opt into plaintext HTTP, per-operation
timeouts (`hindsightRequestTimeoutMs`/`RecallTimeoutMs`/`RetainTimeoutMs`/
`ReflectTimeoutMs`), and optional fail-open turn-start injection
(`memory.hindsightInjection`, bounded + redacted). Responses are capped at 256 KiB
before decoding and 32 KiB rendered; credential shapes are redacted in error
messages. The local store remains keyword-search-backed and model-invoked;
autonomous extraction from turns and embedding search are not implemented
(`crates/pi-coding/src/memory.rs:700-1031`, `docs/src/reference/memory.md`).

### RESOLVED — Hindsight memory backend child-session routing

`Settings.memory.backend = "hindsight"` now propagates through resource-attached
main sessions and into ordinary and persona child sessions. Child factories
inherit the effective `MemoryConfig`, including the configured HTTP endpoint,
bank id/scoping, and injection setting, so `recall`/`retain`/`reflect` tools
use the parent's Hindsight backend instead of silently falling back to local
memory. Coverage includes ordinary/persona children × default/explicit memory
requests with non-default URL/bank/per-project config, plus `off`/`local`
semantics (`crates/pi-coding/src/session.rs:712-730`,
`crates/pi-coding/src/orchestration/runtime.rs:235-295`,
`crates/pi-coding/src/tools.rs:349-380`; focused coverage is exercised by the
`hindsight`-named `pi-coding` library tests in the Verification checklist).

### RESOLVED — skill management view

`/skill <name>` shows a loaded skill's frontmatter (name/description, `globs:` and `alwaysApply` when set) via `skill_frontmatter_summary` (`crates/pi-cli/src/interactive_commands.rs`), dispatched in tui.rs and repl.rs; the existing `/skill:<name>` completion surface is untouched. Unknown names reject actionably.

OMP ships `manage_skill` (create/update/delete managed skills under
`~/.omp/agent/managed-skills/` — `omp://tools/manage_skill.md`). rpi discovers
skills from `.pi/skills`, packages, and `--skill` paths and exposes them as
`/skill:<name>` commands, but no command can author or manage skill files from
the session.

## Missing workflow/interaction modes

### RESOLVED — approval modes enforce typed read/write/exec capabilities

rpi exposes `yolo`, `write`, and `ask` through `--approval-mode`
(`crates/pi-cli/src/args.rs:192-193`) and the global-only `approvalMode`
setting. `yolo` auto-allows every capability; `write` auto-allows read/write
and confirms exec; `ask` confirms all three
(`crates/pi-agent/src/types.rs:141-163`). CLI overrides settings, whose
default is yolo (`crates/pi-cli/src/session_run_blueprint.rs:68,79-82`;
`crates/pi-coding/src/settings.rs:501-504`).

Production tools and extension registrations carry explicit `ToolCapability`
metadata; missing metadata defaults to exec rather than inferring from a name.
The host policy composes ahead of any pre-existing hook
(`crates/pi-cli/src/approval.rs:16-52`); noninteractive modes fail closed
whenever a confirmation is required but no confirmation adapter exists
(`approval.rs:30-31`). TUI confirmations go through the extension UI adapter.
Project `--approve`/`--no-approve` remains a separate resource-trust decision,
not an authorization-mode alias.

### RESOLVED — deterministic auto-mode classifier

rpi now implements a deterministic prompt classifier and emits
`ApplicationEvent::ModeDetected`. `classify_prompt` in
`crates/pi-coding/src/selector.rs:192-218` heuristically classifies user input
as `Question`, `CodeTask`, or `Goal`. `Settings.selector.auto_mode` accepts
`off` (no hints), `suggest` (publishes a status hint after a detected code task
or goal), or `auto` (additionally seeds a todo DAG for detected code tasks when
orchestration is enabled and the todo list is empty)
(`crates/pi-coding/src/selector.rs:31-61,188-262`;
`crates/pi-coding/src/settings.rs:738-739,1908-1939`). The classifier is
checked on prompt submission in `Application`
(`crates/pi-coding/src/application.rs:1458-1479`), producing `ModeDetected`
events that the TUI can render. Tests cover fixture-based classification,
case-insensitivity, mode hints, auto-mode serde, settings round-trip, and
suggest/auto/off behavior including todo DAG seeding
(`crates/pi-coding/src/selector.rs:1708-1793`;
`crates/pi-coding/src/application.rs:3697-3790`).

The LLM-backed classifier (grok's `classifier_model`/`classify_timeout`) is
not implemented; classification is rule-based and does not degrade to asking.

### RESOLVED — user-facing prompt queue management

`/queue` views pending steering/follow-up items (counts + previews) and `/queue cancel` drains them (`crates/pi-cli/src/tui.rs` `dispatch_queue_command`, repl.rs arm; builtin in interactive_commands.rs). Covered by live-Application tests.

rpi has a dual `steering` and `follow_up` message queue inside the agent loop,
governed by `QueueMode::OneAtATime` or `QueueMode::All`
(`crates/pi-agent/src/queue.rs:1-55`, `crates/pi-agent/src/agent.rs:247-250`).
`Application` exposes `queued_messages`/`drain_queued_messages` for inspection
(`crates/pi-coding/src/application.rs:1552-1559`), and the TUI's
`app.message.dequeue` keybinding action (default Alt+Up) drains queued steering
and follow-up messages back into the editor
(`crates/pi-cli/src/keybindings.rs:67,768`; `crates/pi-cli/src/tui.rs:6567-6575`).

The gap versus Grok's prompt queue is the user-facing management surface:
there is no enqueue/removal/reorder/edit/clear/send-now UI, no interruptible
wait barrier, and no persistence of queued prompts across restarts
(`analysis.md:1271-1273,1300-1303`). The current queues are internal scheduler
inputs rather than a standalone user prompt queue.

### RESOLVED — doom-loop recovery

Consecutive identical (tool, error-prefix) failures stop the turn after 3 (`DOOM_LOOP_THRESHOLD`) with an actionable message; transient network errors never count; per-turn scoping (`crates/pi-coding/src/session.rs` `doom_loop_recovery` in `compose_host_post_tool_call`). 6 tests.

### RESOLVED — idle AI suggestions

`next_suggestion` derives a deterministic one-line suggestion (`/queue`, `/goal`, `/workflow list`) from state when idle, rendered dim in the status line, never outranking busy/steering/ask (`crates/pi-cli/src/tui.rs`). 2 tests.

Grok ships `[suggestions]` (AI-generated shell suggestions via
AISuggest/SuggestPrompt side channels — `analysis.md:862,3677`). rpi has no
model-generated suggestion surface; only static command completion and typo
hinting exists.

### By design — rpi workflows are worktree/YAML; grok/OMP use script engines

Grok's workflow engine is native Rhai scripting (`.grok/workflows/*.rhai`,
host API: `agent`/`parallel`/`phase`/`complete`/`pause`/`await_user`/`budget`/
`write_scratch_file`/`json_encode`, budgets 1-1024, `validate_only`,
`resume_from_run_id` — `analysis.md:4332-4361`). rpi workflows are
declarative YAML executed in git worktrees (`crates/pi-coding/src/workflow_worktree/`).
Both are valid designs; the gap is expressiveness (conditional/loop/user-gate
workflows are impossible in rpi's YAML DAG).

### RESOLVED — role/persona contracts

`AgentDefinition` gains optional `max_turns`/`max_tool_calls`/`timeout_secs`/`disallowed_tools`/`capability_ceiling` (camelCase + kebab-case frontmatter, decode-compatible, zero/non-numeric rejected — `crates/pi-coding/src/orchestration/definitions.rs:37-91`). Enforced at child spawn/runtime: per-child turn/tool counters via `definition_contract_stop_hook` (settles Failed with reason), per-run wall-clock deadline with abort, disallowed-tool filtering and `ToolCapability` ceiling at the production child factory (`runtime.rs`). `/role` lists/details/selects roles for the next unnamed `task` spawn (`interactive_commands.rs`, tui.rs/repl.rs dispatch; preference via `OrchestrationRuntime::set_preferred_agent`). Tests cover decode, enforcement, filtering, ceiling, and the /role surface.

Grok ships roles (`.grok/roles/*.toml`: model/effort/capability ceiling/
prompt file) and personas (behavior override + input/output contracts), with
AgentDefinition frontmatter (effort/promptMode/disallowedTools/maxTurns/
maxToolCalls/timeoutSecs/finalizeGraceSecs — `analysis.md:4298-4324`).

### RESOLVED — goal role-model pins

`Goal.pins: Vec<String>` with `/goal pin <text>|pins|unpin <index>` (bounds: 8 pins × 200 chars), journal-persisted via `GoalEventKind::PinsUpdated` (replay-validated, terminal-gated, fork-cloned), projected into the goal turn context as `Role-model pins:` (`crates/pi-coding/src/goal.rs`, session.rs goal_context_message). 5 goal + 2 application + 2 cli + 1 dispatch tests.

Grok's `[goal]` pins role models (planner/strategist/skeptic) and tracks
streaks/classifier state (`analysis.md:820,3742-3746`).

## Missing collaboration/extension infrastructure

### RESOLVED — MCP client and `mcp` tool

rpi now implements a session-scoped MCP client and registers an `mcp` tool.
Servers are declared under `Settings.mcp_servers` in the Grok-compatible
`[mcp_servers.<name>]` shape (`crates/pi-coding/src/settings.rs:595-620,752-754`).
The stdio transport speaks JSON-RPC 2.0 over a child process with
Content-Length framing; the registry spawns clients lazily, tears them down on
drop, and respawns after errors (`crates/pi-coding/src/mcp.rs:1-83,489-796`).
Tool actions are `list_servers`, `list_tools`, and `call`
(`mcp.rs:489-493,806-1541`). The tool is listed in `TOOL_NAMES` and wired into
the coding factories (`crates/pi-coding/src/tools.rs:132,293,497,523,554`).
Tests cover framing, fake stdio server round-trips, tool listing/calling,
unknown-tool validation, abort handling, session lifecycle, and SSE transport
limitation reporting (`crates/pi-coding/src/mcp.rs:806-1541`).

Full gateway features are now implemented: per-entry `disabled` lists
(`mcpServers[].disabled`, Cursor-compatible; Claude Desktop `disabledMCPServers`
mapped on import), Claude/Cursor config import (`rpi mcp list|import
[--source claude|cursor|auto] [--force]` via `mcp_import.rs`), progressive
`search_tool` discovery (lazy `tools/search` with full-list fallback), OMP's
250 ms fast-start spawn gate, and bounded reconnect with capped exponential
backoff (3 attempts, then actionable errors with redacted stderr)
(`crates/pi-coding/src/mcp.rs:393-614`, `mcp_import.rs:31-308`).

### RESOLVED — host-level hooks system

rpi now implements host-level hooks fired at session, turn, and tool-call
events. The executor lives in `crates/pi-coding/src/hooks.rs`: `HostHooks::new`
binds configured entries to a session (`hooks.rs:91-93`), and `HostHooks::fire`
spawns each hook as a tokio process group, writes a JSON payload to stdin,
caps stdout at 64 KiB, applies a per-hook timeout (default 5 s, capped at
60 s) with `killpg` cleanup, and fails open for `pre_tool_call` unless the
entry sets `failClosed: true` (`hooks.rs:120-265`; `hooks.rs:395-400`).

Settings are typed under `Settings.hooks`
(`crates/pi-coding/src/settings.rs:556-557`) with `HookEvent`
`PreToolCall`/`PostToolCall`/`SessionStart`/`SessionEnd`/`TurnStart`/`TurnEnd`
(`settings.rs:372-390`) and `HookConfig` fields `event`/`matcher`/`command`/
`timeoutMs`/`enabled`/`failClosed` plus an `extra` flatten map for unknown
fields (`settings.rs:392-425`). The catalog exposes `hooks` under
`TrustSecurity` with `RELOAD` apply behavior
(`crates/pi-coding/src/settings_catalog.rs:307`).

Firing points: `Session::set_host_hooks` builds and installs `HostHooks`
(`crates/pi-coding/src/session.rs:2649-2658`); `compose_host_pre_tool_call`
runs first and can block a tool (`session.rs:3660-3686`);
`compose_host_post_tool_call` runs last and is advisory
(`session.rs:3690-3725`); `session_start` fires lazily on first run
(`session.rs:3489-3499`); `session_end` fires asynchronously on Drop
(`session.rs:3594-3612`); `turn_start`/`turn_end` fire around the model turn
(`session.rs:3500-3501,3555-3556`). Extension tool calls are excluded: the
runtime records extension tool names via
`Application::bind_runtime_generation`
(`crates/pi-coding/src/application.rs:594-598`) and `Session::set_extension_tool_names`
(`session.rs:2660-2665`) so host hooks skip them.

Tests cover blocking, fail-open/fail-closed, timeout, extension exclusion, and
the session/turn lifecycle firing sequence
(`session.rs:4792-5110`; `hooks.rs:489-640`).

### RESOLVED — plugin marketplace

`rpi plugin list|install|remove|update` with local-dir, tarball, and GitHub `owner/repo` sources, a local-or-URL marketplace index (`pluginMarketplace` setting, JSON list validated with 4096-entry/8 MiB caps), manifest validation against the pi-extension.json schema before anything is loadable, atomic install/swap, and a trust-store gate on loading (install records Trusted; remove clears it). No code executes during install/update; archives harden against traversal/symlinks with byte caps (`crates/pi-coding/src/plugin.rs`; `plugin_commands.rs`; resource-manager scan). 30+ tests.

Grok ships plugins + marketplace (`[plugins]`, `[[marketplace.sources]]`,
`grok plugin` — `analysis.md:964-966,863,1049`); OMP ships a marketplace and
plugin manager (`omp://marketplace.md`, `plugin-manager-installer-plumbing.md`).
rpi has extension loading but no packaged-plugin market or install command.

### RESOLVED — encrypted live collaboration

rpi implements the encrypted live collaboration wire protocol under
`rpi --listen`: AES-256-GCM encrypted binary frames, HKDF-SHA256 key derivation
per room epoch, capability-hash subprotocol authentication, control/view role
links, and the `collab_start/status/stop` RPC surface
(`crates/pi-cli/src/modes/collab_service.rs`, `listen.rs`, `rpc.rs`). The
interactive `/collab` command is wired in the TUI/REPL dispatcher
(`crates/pi-cli/src/interactive_commands.rs`), and `/join`/`/leave` handle
guest links. Server-side snapshots and live events are projected through a
privacy-redaction pass that strips secrets and host paths before they reach
guests; the browser guest reads the capability key from the URL fragment
locally and never leaks it in evidence. CLI and browser guest clients exist
(`E2E.d/collab/collab_guest.mjs`, `crates/pi-cli/web/src/CollabGuestView.tsx`,
`collab.ts`). The full E2E scenario passes: host + two CLI guests + one
Playwright browser guest through join, encrypted snapshot/event exchange, role
enforcement, prompt/abort, disconnect/rejoin, and host stop, with zero
lingering processes. Orchestrator assertions SO-01..09 pass; all control, view,
and browser guest assertions pass (evidence under `$EVIDENCE_ROOT/collab`).

### RESOLVED — Agent Client Protocol (ACP v1)

`rpi agent stdio` (JSON-RPC 2.0, Content-Length framing via shared `framing.rs`) and `rpi agent serve [--address]` (local WebSocket): initialize/authenticate/session(new|prompt|cancel|close)/logout + `session/update` notifications (message/thinking/tool_call/usage) + reverse request `session/request_permission` (allow/reject routed to path-level permissionRules then capability-wide approval; cancelled on session/cancel). Each ACP session wraps a real `Application` (client cwd, model resolution, session recording); ACP ErrorCode surface (-32700/-32601/-32602/-32000/-32002). `modes/acp.rs` + additive blueprint `acp_approval` hook. 17 tests incl. initialize handshake, version negotiation, reverse-request round trip, cancel.

Grok converges all clients on the Agent Client Protocol (ACP: initialize/
authenticate/session/prompt + reverse requests; `grok agent stdio` for editor
embedding, `grok agent serve` WebSocket, leader mode — `analysis.md:1960-1964,
543-567,992,1211-1218`). rpi's JSONL RPC (`rpi rpc` / `--mode rpc`) is not
ACP-compatible; editors that speak ACP must use `rpi agent stdio`/`serve`.

### RESOLVED — MCP/ACP transport test sufficiency

The MCP client's transport edge cases are now locked by tests:
Content-Length framing, `tools/list` pagination followed through `nextCursor`
(two-page fixture aggregating and caching the union), reconnect with capped
exponential backoff (100 ms → 1 s, 3 attempts, then actionable errors with
redacted stderr), and progressive `search_tool` discovery with full-list
fallback (`crates/pi-coding/src/mcp.rs:95-101,348-373,393-418`; pagination
test at `mcp.rs:1734-1763`). The ACP surface covers the JSON-RPC parse-error
code: malformed JSON in an ACP message returns `-32700` with the message id
preserved (`crates/pi-cli/src/modes/acp.rs:106-110,1299-1303`).

### RESOLVED — plugin git sources and npm integrity fail-closed

`rpi plugin install` now accepts git URL sources — `git+https://host/owner/repo`,
`git+ssh://git@host/owner/repo.git`, `https://host/owner/repo.git`,
`ssh://git@host/owner/repo.git`, scp-like `git@host:owner/repo.git`, and
`file://`/`git+file://` for local mirrors — cloned shallow with argv-built
commands (no shell string is ever involved; no code executes during install)
(`crates/pi-coding/src/plugin.rs:16-20,764-825`). npm references
(`npm:<name>[@<version>]`) are fetched from the registry and verified against
`dist.integrity`: only `sha512` is accepted and any malformed digest or
mismatch fails closed; metadata without integrity (or with only the legacy
`dist.shasum`) refuses to install unauthenticated content, and tarballs over
plain-http are refused unless the registry is explicitly configured
(`plugin.rs:1075-1195`; tests at `plugin.rs:2497-2541,2661-2703`). This is
the plugin-marketplace surface only — the resource-package model still
rejects `npm:` sources (see `By design — npm: package source rejected`).
[Pending decision: the user has indicated npm plugin support may not be
needed; a removal would be tracked separately — this entry documents the
implemented state.]

## Missing config/CLI surface

### RESOLVED — config profiles

`--profile <name>` (global flag; `PI_PROFILE` env honored, CLI wins) relocates the user base dir to `<base>/profiles/<name>` — settings.json, auth.json, models.json, sessions/, memory/, skills/, workflows/, trust store all derive from the relocated agent base (`crates/pi-cli/src/session_run.rs` `activate_profile`; `resources.rs` `apply_profile` via OnceLock; `session_store_agent_base`; `models_config.rs`). `--profile default` = no relocation. Name validation: 1-64 ASCII alnum + `-`/`_`, actionable errors. 5 integration + unit tests incl. per-profile isolation of sessions/skills/auth.

OMP ships `--profile <name>` relocating the user base dir (`omp://config-usage.md:76-78`).
Grok ships managed config (`/etc/grok/managed_config.toml` +
`requirements.toml`, fail-closed, Ed25519-signed policy envelopes —
`analysis.md:676-680,1075,2787`), macOS MDM preferences, and remote campaign
overlays (`GROK_CAMPAIGNS_OVERRIDE`, `campaigns_state.json` FIFO —
`analysis.md:701-709`). rpi has a single JSON settings file.

### RESOLVED — TOML config and env expansion

Settings support TOML by deterministic extension rule (`settings.toml` sibling wins; `.toml` → TOML, else JSON — never content-guessed) with full read+write round-trip, plus `$VAR`/`${VAR}` expansion applied to string values (incl. nested `Vec<String>`) at load time over the parsed JSON document before deserialization — missing vars stay literal with a deduped diagnostic (`crates/pi-coding/src/settings.rs` `load_settings_file_with`/`expand_env_in_value_with`/`expand_env_string_with`; `toml` crate added). Precedence unchanged (session overrides still win). 8 tests incl. sibling preference, extension rule, nested collections, missing-var reporting, override precedence.

Grok config is TOML with `$VAR`/`${VAR}` expansion and `[[version_overrides]]`
(`analysis.md:667-668`). rpi uses JSON `settings.json` with no env expansion
or versioned overrides.

### RESOLVED — `doctor`/`setup`/`dashboard` subcommands

`rpi doctor` (9 PASS/WARN/FAIL checks, `--json`, secret-free), `rpi setup` (prints models.json/auth.json paths + guidance), `rpi dashboard` (session counts, latest session, goal state, tool list) at `crates/pi-cli/src/doctor.rs` + args.rs Command variants + lib.rs dispatch. 12 tests incl. a no-secret-leakage fixture.

Grok ships `grok inspect` (full config check — `analysis.md:1014-1041`),
`doctor` (environment diagnostics — `:1053`), `setup` (fetch managed config —
`:1047`), `dashboard` (`:1056,798`), and `memory`/`mcp`/`plugin`/`leader`/
`workspace` management families (`:1048-1056`). rpi's CLI subcommands include
`login`/`logout`/`models`/`sessions`/`import-session`/`reload`/`export`/
`install`/`remove`/`list`/`config`/`update`/`llama`/`doctor`/`setup`/`dashboard`
(`crates/pi-cli/src/args.rs:222-333`), with no inspection or diagnostics
command.

### RESOLVED — shell completion generation

rpi now ships `rpi completion bash|zsh|fish` via `clap_complete`.
`CompletionShell` maps to the underlying `clap_complete::Shell`,
`write_completion` generates the script, the `Command::Completion` variant
is parsed by the CLI, and the dispatcher writes the script to stdout
(`crates/pi-cli/src/args.rs:220-247,354-359`;
`crates/pi-cli/src/lib.rs:59,136-139`). Tests verify that unsupported shells
are rejected and that each supported shell produces a non-empty script
mentioning `rpi` and `--help` (`args.rs:816-849`).

### RESOLVED — headless structured output (already existed)

Verified already satisfied: `--print`/`--mode json` (modes/json.rs JSONL event stream) and `--mode rpc` / `rpi rpc` (modes/rpc.rs JSONL RPC control plane, smoke-verified round-trip) — integration suites tests/json_mode_binary.rs and tests/rpc_binary.rs.

Grok headless supports JSON Schema structured output with pre-run validation
(`analysis.md:514-517`). rpi headless has `--mode text|json|rpc` (plain text,
JSON event stream, JSONL RPC — `crates/pi-cli/src/args.rs:43-45,199-205`) but
no schema-validated structured output.

### RESOLVED — scoped provider credential store

auth.json gains a backward-compatible reserved `scopes` section (flat files load unchanged as unscoped; `provider "scopes"` disambiguated by shape) with `rpi login|logout --scope <label>`, active-scope resolution (`PI_AUTH_SCOPE` env > `authScope` settings key), scope-match-wins/unscoped-fallback semantics, and secret-free errors (`crates/pi-coding/src/auth.rs`; pi-cli snapshot mirrors in models_config.rs). Covered by lib + 14 integration auth tests.

The gap versus Grok is the data model, not durability: rpi writes `auth.json`
through advisory locking and atomic temp+rename. `AuthFileLock::acquire`
creates a per-file `auth.json.lock` owner record, waits with timeout/retry,
and reclaims stale locks by detecting a dead pid, a mismatched host/boot
identity, or a different process start time
(`crates/pi-coding/src/auth.rs:285-340,275-292,434-467`); the lock is released
via RAII (`auth.rs:342-348`). `write_credentials_atomic` writes to a temp
file, syncs, and renames it over the target (`auth.rs:1050-1081`). Focused
regressions verify exclusive access, stale-lock recovery across process
restarts, and no token leakage in lock files (`auth.rs:1239-1477`).

The remaining gap versus Grok is the data model: rpi stores a flat map of
credentials rather than a scoped store (BTreeMap scope→GrokAuth with OIDC/device/
WebLogin classifications) and does not isolate corrupt reads per scope
(`analysis.md:897-906,828,907-908`).

## Missing session/conversation features

### RESOLVED — session TTL cleanup prunes expired native sessions

`prune_expired_sessions` walks the native session tree at startup and deletes
`*.jsonl` files whose mtime is older than the TTL plus a 1-hour active-session
grace, while skipping symlinks, unreadable metadata, future mtimes, the current
session file, and foreign sources (`crates/pi-coding/src/session_store.rs:1871-1903`). The default TTL is 30 days (`DEFAULT_SESSION_TTL_DAYS=30`,
`session_store.rs:1003`) and is overridable via `Settings.session_ttl_days`
(`crates/pi-coding/src/settings.rs:673`; catalog `sessionTtlDays` at
`crates/pi-coding/src/settings_catalog.rs:245`). `session_run.rs` resolves the
current session before cleanup and passes it in the skip list
(`crates/pi-cli/src/session_run.rs:654-680`). Tests cover TTL fallback,
zero-value defense, the active grace window, skip semantics, and symlink and
future-mtime safety (`session_store.rs:3767-3854`;
`session_run.rs:907-924`).

### RESOLVED — rewind and checkpoint

`/rewind <entry-index|checkpoint-name>` truncates the session file at the cut record's byte offset (archiving the dropped tail to `<file>.rewind-<ts>.jsonl`), rebuilds recorder + transcript, restores todo from the latest surviving snapshot and goal by journal replay, and resets the Responses chain (`crates/pi-coding/src/session_store.rs` `rewind_to`, `session.rs` `rewind`). Safety: refuses past the first entry / at end / while any orchestration job, active workflow, or bash is running (`rewind_refusal`). `/checkpoint <name>` marks a side record (never joins the transcript chain) targetable by rewind. 10+ store/session/application/cli tests.

Grok supports `rewind` session rollback (`analysis.md:1265`); OMP ships
`checkpoint`/`rewind` for collapsing exploratory context (`omp://tools/checkpoint.md`).
rpi has no rewind; the closest is forking a session.

### RESOLVED — snapcompact and useless-result elision

`/compact --snap` (alias `/snapcompact`) archives dense history deterministically WITHOUT a provider call: keeps the last K user turns verbatim (`compaction.snapKeepTurns`, default 10, cut always on a user message), replaces the older region with a structured summary (type counts, char span, timestamps, tool names ≤20, user-ask first-lines ≤10), and writes the lossless original tail to `<file>.snapcompact-<ts>.jsonl` (`crates/pi-coding/src/compaction.rs` `find_snap_cut_point`/`build_snapcompact_summary`, `session.rs` `compact_snap`). Useless-result elision (empty/whitespace or exact duplicate of the preceding error) applies during ANY compaction with an `[elided N useless results]` note. 6 snap + 22 compaction tests.

OMP ships snapcompact (archive dense history without LLM summary —
`omp://compaction.md:133-135`), useless-result elision
(`:165-167`), split-turn compaction (`:194-196`), and file-operation
context in summaries (`:254-256`). rpi compacts via LLM summary only
(`crates/pi-coding/src/compaction.rs:57-68`, `crates/pi-coding/src/session.rs:1891-1910`).

### RESOLVED — `/fresh`, `/dump`, and encrypted share

`/fresh` (new session, current archived), `/dump [--jsonl] [path]` (HTML default / JSONL via existing export path), and `/share --encrypt [passphrase]` (AES-256-GCM via `ring` — already a workspace dep — key = SHA-256(passphrase), 12-byte nonce prefix, plaintext never stored; `crates/pi-coding/src/encrypt.rs` + share.rs). 8 encrypt + 2 share + 5 cli tests incl. round-trip and wrong-passphrase rejection.

OMP ships `/fresh` (reset provider session without losing local transcript),
`/dump` (plain-text copy to clipboard), and E2E-encrypted `/share`
(AES-256-GCM + gist/blob + viewer page —
`omp://session-operations-export-share-fork-resume.md`). rpi's `/share`
(`crates/pi-coding/src/share.rs:180-183`, invoked from
`crates/pi-coding/src/application.rs:2561-2571`) exports the full raw
transcript to HTML and uploads it as a GitHub **secret** gist without
encryption. Anyone who obtains the URL — and GitHub itself — can read the
entire conversation in plaintext; this is privacy-significant for sessions
containing API keys or credentials.

### RESOLVED — handoff generation

`generate_handoff` builds a deterministic envelope (session/model/cwd/git branch+dirty, goal lifecycle + remaining tokens, todo counts, running jobs, last 5 user asks, next-step hints) — zero model calls — plus optional summarizer prose via the shared `run_summary_provider_call` (`crates/pi-coding/src/handoff.rs`). `/handoff` renders the block and copies to clipboard (`interactive_commands.rs`, tui.rs/repl.rs). 5 handoff + dispatch tests.

OMP generates cross-session handoff summaries (`generateHandoff()` —
`omp://compaction.md:248-250`). rpi has no handoff concept.

### RESOLVED — transcript-level secrets obfuscation

Credential-shaped text (ghp_, sk-, AKIA, Bearer, private keys, token=/access_token=) is redacted to `[REDACTED]` at render/export time — TUI transcript (tui.rs render paths), print mode (human_event_renderer.rs), and HTML/markdown export (`export/mod.rs` escape_text, `export/markdown.rs`) — while session storage keeps raw text. 7 tests; existing transcript/export tests unaffected.

OMP obfuscates secrets with deterministic reversible placeholders
(`omp://secrets.md`). rpi stores raw secrets verbatim in session JSONL files
and in HTML exports and shared gists; only TUI/debug presentation redacts
credential-shaped text such as AWS keys and `token=`/`password=` values
(`crates/pi-coding/src/tool_presentation.rs:888-915`). Anyone with filesystem
access to `<agent-dir>/sessions/` — or a share URL — can recover credentials
in plaintext.

## Workflow, delegation, and TUI rendering

### RESOLVED — CJK/Unicode delegation intent routing with typed agent contracts

Delegation detection now recognizes natural-language intent, including Chinese
constructions: `delegation_intent` combines an English delegation-verb check
with `cjk_delegation_construction`, which matches single-character tokens
(`让`/`请`/`叫`/`派`) directly abutting the agent name and two-character tokens
(`安排`/`委托`/`交给`) ending immediately before it
(`crates/pi-coding/src/orchestration/runtime.rs:4810-4825,4891-4903`). Exact
agent mentions route only when `delegation_intent` holds (`runtime.rs:3659-3663`),
and candidate extraction scans ASCII identifier runs (`runtime.rs:5044-5052`).
The CJK intent matrix is locked by `delegation_intent_tests`
(`runtime.rs:6222-6276`).

Routing is typed end to end. Workflow Todo items carry an explicit `agent`
field; `resolve_task_agent` (`runtime.rs:3572-3613`) honors explicit agent >
exact mentions in the task content > ranked selection, failing actionably on
missing/disabled/ambiguous agents and rolling the Todo state back on error
(`crates/pi-coding/src/application/todo_execution.rs:357-377`). The spawned
`TaskItem` carries the resolved `agent` and the originating `todo_task_id`
(`runtime.rs:629-635`). rpi now bundles a `researcher` agent alongside `task`
(`crates/pi-coding/src/orchestration/definitions.rs:503-541`), and workflow
planning validates objective-assigned agents against the catalog before
planning starts (`crates/pi-coding/src/workflow/supervisor.rs:866-867`;
`runtime.rs:3696-3743` `validate_delegation_agents`), so a workflow naming an
undefined or disabled agent fails with an actionable catalog diagnostic
instead of silently falling back.

### RESOLVED — workflow panel: lean DAG rows, agents-first detail, content-aware scrolling, silent close

The workflow detail pane leads with live actors: Active tasks
(`workflow_panel.rs:853`) → Agents (`workflow_panel.rs:884`, collapsible
activity feeds) → Todo DAG (`workflow_panel.rs:919-946`); the ordering
`Active < Agents < Todo DAG` is asserted by a render regression
(`workflow_panel.rs:1417-1418`). The Todo DAG rows are deliberately lean —
phase + task content + status bullet, with the opaque task id and `ready`
marker never rendered — and in-progress tasks carry a compact subagent
association mapped by task id with a fallback to the planned `agent` role
(`crates/pi-cli/src/todo_dag_view.rs:142-193`).

Detail-pane navigation is content-aware: `navigation_focus` routes ↑/↓/j/k to
the detail when it overflows and to the list otherwise, Tab flips an explicit
focus, and detail scrolling clamps at the content edge
(`crates/pi-cli/src/workflow_panel.rs:410-416,505-540`). Closing the workflow
panel is silent: a live workflow status line is parked in
`deferred_workflow_status` while the panel owns the page and discarded on
close, so it never resurfaces as an "Error: Workflow … · running" toast after
the panel closes (`crates/pi-cli/src/tui.rs:2394-2400,3753-3758`).

### RESOLVED — markdown fence completeness, answer-card borders, comment marker, fold visibility

Unclosed code fences render a complete frame in both renderers: the shared
markdown renderer closes a still-open fence with a temporary bottom border
carrying the `… (unclosed fence)` marker instead of faking a closed block
(`crates/pi-coding/src/markdown/render.rs:465-531,847-936`), and the TUI's
manual markdown path mirrors it via `push_code_frame_bottom`
(`crates/pi-cli/src/tui.rs:13478-13496`, goldens at `tui.rs:15845-15900`).
Manual-frame borders were unified to exactly the frame width in the same pass
(`tui.rs:13375,13460,13508`).

In the code-review panel, the streaming Answer card's labeled top border now
shares one width with its content and bottom rows (`render_diff_line`
MarkdownBody branch, `crates/pi-cli/src/code_review_panel.rs:2214-2240`), the
`◆` decoration on comment headers is removed (`push_annotation_card`,
`code_review_panel.rs:2080`), and Space-fold hides only unchanged context —
every +/- changed line stays visible under the folded header, which counts
both (`{hunk} · N lines folded · M changed`,
`code_review_panel.rs:1988-1999,2157-2163`). Regression tests assert the exact
folded header and the terminal-pane rendering with changes visible
(`code_review_panel.rs:3468-3649`).

### RESOLVED — over-limit mermaid diagrams split into numbered bounded panels

Flowcharts and class diagrams over the per-chunk budget are split by
`greedy_chunk_of` into bounded chunks (a node is never split across chunks),
and each chunk renders as its own bordered frame numbered `[part i/n]` with
stub notes on crossing edges (`crates/pi-coding/src/markdown/mermaid.rs:298-350,768-773`;
`crates/pi-coding/src/markdown/render.rs:397-415`). The 112-node split golden
asserts two numbered frames, the single crossing edge as source and target
stubs, and no source fallback (`render.rs:1069-1108`); chunk bounds and the
over-total-ceiling fail-closed diagnostic are covered in mermaid.rs
(`mermaid.rs:1904-1943,1966-2021`).

Width-sweep stability tests for the split renderer are PENDING (D42 has not
landed); the goldens above are fixed-width.

### RESOLVED — compaction A→B report estimates the post-compaction context

Both LLM and snap compaction now report a genuine post-compaction estimate:
`estimated_tokens_after = estimate_context_tokens(&compacted)` — a pure
heuristic over the actual summary + kept tail — instead of reusing the
usage-aware estimator, whose anchor is the last assistant turn's real usage
measured against the FULL pre-compaction context and therefore survives the
cut unchanged (A would always equal B)
(`crates/pi-coding/src/session.rs:2380,2451`; `compaction.rs:25-41`).
`CompactionResult.estimated_tokens_after` is recorded and surfaced as
`Compacted {A} → {B} estimated tokens`. Regression tests prove
`estimated_tokens_after < tokens_before` for both paths and pin the exact
status line (`session.rs:6730-6740,6822-6827`).

### RESOLVED — planning: wall-clock deadline, live activity with outcomes, concise task titles

Workflow planning is bounded by a real wall-clock deadline (default 90 s,
`PLANNING_DEFAULT_DEADLINE`, overridable per factory via
`set_workflow_planning_deadline`), and the actor aborts the in-flight prompt
and settles from canonical Todo state when it expires — a distinct `TimedOut`
outcome from budget stops (`crates/pi-coding/src/workflow/supervisor.rs:38-67`;
`crates/pi-coding/src/application/workflows.rs:139-147`).

The workflow page shows live planning activity instead of a static spinner:
supervisor tool starts render a bounded, credential-redacted one-line summary
with the argument fragment that carries intent (bash command after `$`,
todo/goal op, read/edit/write path), completed calls append `· ok`/`· err`,
and thinking/text deltas are coalesced into bounded chunks
(`crates/pi-coding/src/application/workflows.rs:230-333`); the panel's
placeholder falls back to the static text only when the feed is genuinely
empty (`crates/pi-cli/src/workflow_panel.rs:920-948`).

The planning prompt now requires concise ~60-character imperative task titles
and instructs the model to preserve objective-assigned roles as a typed
`agents` array parallel to `items`
(`crates/pi-coding/src/workflow/supervisor.rs:154-165`); the `todo` tool
description documents the durable DAG contract
(`crates/pi-coding/src/tools.rs:711`).

### RESOLVED — security hardening: brush host-pid guard, npm integrity fail-closed, scrollback root cause

The embedded brush shell's guarded `kill` scans the argument list for a
numeric target pid (skipping `%` job specs and `-` signal specs) and refuses
to signal the rpi host pid; `exec`/`suspend`/`ulimit`/`umask` stay refused
(`crates/pi-coding/src/tools/bash/brush.rs:44-49,375-462`). The refusal is
covered by the host-pid kill regression
(`crates/pi-coding/src/tools.rs:4740-4745`).

`rpi plugin install` from an npm registry verifies the tarball against the
registry's `dist.integrity` sha512 digest and fails closed on any other
algorithm, malformed digest, or mismatch — content fetched over plain-http
registries is refused unless the registry is explicitly configured
(`crates/pi-coding/src/plugin.rs:770-879`).

Scrollback-loss root cause fixed: the transcript `Paragraph` no longer
word-wraps. The renderers pre-wrap every row to the exact pane width, and
ratatui's `WordWrapper` was splitting the user card's whitespace-only padding
rows into extra blank rows (`crates/pi-cli/src/tui.rs:10593-10598`).

Todo-DAG rows are redacted before rendering: `redacted_sanitize` runs the
shared `redact_secrets` pass on every agent/task value, so credential-shaped
text never reaches the panel even though the durable workflow store persists
it unredacted (`crates/pi-cli/src/todo_dag_view.rs:125-128,190`).

### RESOLVED — read-card renders source-exact indentation

`read` tool cards render file lines verbatim after the `│ ` frame: leading
spaces are preserved exactly (tabs expanded to four spaces), with no padding
layer, dedent, or re-flowed continuation rows, via the shared
`push_tool_box_row`/`push_tool_box_line` row builders
(`crates/pi-cli/src/tui.rs:12317-12336`). Regressions cover the 2/8-space
fixture, the user-reported impl block, and an end-to-end read-tool round trip
(`tui.rs:28461-28740`).

### RESOLVED — OMP-style task tool: batch context, per-item output contracts, delivered-payload validation

The `task` tool's batch mode now REQUIRES `context`, rendered into every
child's system prompt as a `<context>` section, and exposes per-item (and
top-level) `outputSchema`/`schemaMode` fields
(`crates/pi-coding/src/orchestration/tools.rs:588-629`; child-prompt assembly
at `runtime.rs:3775-3786`). The run loop validates each child's delivered
`yield` payload against the schema and reports the outcome as
`TaskResult.structuredOutput` — `"permissive"` (report-only, default) or
`"strict"` (a validation failure surfaces as a job error) — with the parsed
payload retained for parent inspection
(`crates/pi-coding/src/orchestration/runtime.rs:2044-2049,2094-2098,758,
766-778`). Tests cover conforming payloads per child, permissive
non-conformance without job failure, strict failure surfacing, the rendered
`<context>` section in every child prompt, and the schema exposing the
output-contract fields (`runtime.rs:7138-7239,7243-7303`;
`orchestration/tools.rs:1133-1137`).

### RESOLVED — unknown declared tools are silently ignored with a deduped warning

Agent definitions that declare tools outside the child-injectable set no
longer fail: unknown names are reported once per (agent, tool) pair as a
deduped, non-fatal warning and silently dropped — the declaration is never
injected and never makes the agent unavailable (OMP-aligned)
(`crates/pi-coding/src/orchestration/definitions.rs:258-295`; runtime dedup
set at `runtime.rs:824-828,1180-1183`). Tests assert spawn succeeds with
exactly one warning per unknown tool, batches with one unknown item still
spawn every item, and no unknown tool ever reaches the injected child set
(`runtime.rs:7484-7617`).

### RESOLVED — `yield` is a valid declared child tool

`CHILD_PLUMBING_TOOLS` now includes `yield` alongside the auto-provided
todo/process/task/hub/goal plumbing, so a `yield` declaration in an agent
definition is accepted as redundant rather than rejected, and injection stays
idempotent (`crates/pi-coding/src/orchestration/definitions.rs:245-254`). The
validator accepts it only because it is auto-provided child plumbing — it
never leaks into the main-session built-in set (`definitions.rs:1367-1371`).

### RESOLVED — durable todo snapshot persistence with resume restore and malformed rejection

Todo state persists through the session file: `record_todo_snapshot` writes a
durable `todo_snapshot` record via `append_entry_durable` (sync + atomic
replacement, so the state survives an unclean exit), and resume/fork/reload
restore the latest snapshot's open tasks
(`crates/pi-coding/src/session_store.rs:854-860,2330`;
`session.rs:2543-2546,2941-2944`). Malformed `todo_snapshot` records fail
load AND resume with an actionable "decoding todo_snapshot <id>" error
instead of silently corrupting the DAG (`session_store.rs:3328-3366`). Tests
cover snapshot round-trip, active-branch following, legacy-snapshot id
migration, and malformed rejection (`session_store.rs:3911-3985`; resume
restore at `session.rs:5606-5608`).

### RESOLVED — todo state is session-isolated

A fresh recording starts with an empty todo, and a fork copies the todo at
the fork point into an independent file whose later mutations never touch the
source session — there is no global or project-level todo store
(`crates/pi-coding/src/session.rs:8214-8218`). Workflow planning todos run in
the workflow's own child session and never leak into — or clobber — the main
session's todo (`crates/pi-coding/tests/workflow_application.rs:1507-1516`);
the DAG lifecycle suite verifies restore isolation across sessions
(`crates/pi-coding/tests/todo_dag_lifecycle.rs:602-606`).

### RESOLVED — workflow todo parallelism: width-first planning and a main-view worker strip

The planning prompt now requires a WIDE, parallel DAG — independent work in
separate tasks with no dependency edges and several ready tasks per execution
wave — because the executor runs every ready task concurrently up to its
limit; `depends_on` is reserved for genuine data/control dependencies
(`crates/pi-coding/src/workflow/supervisor.rs:154-168,183-193`). The guidance
is locked by `initial_prompt_requires_parallel_width_guidance`
(`supervisor.rs:1478-1500`). The main TUI view projects live workflow workers
— `◈ <agent> · <current task>` per running workflow, supervisor excluded,
bounded and collapsed `+N more` — in the TodoHUD strip so concurrency is
visible without opening `/workflow` (`crates/pi-cli/src/tui.rs:12730-12787`;
tests at `tui.rs:23459-23522`).

### RESOLVED — OMP-style working indicator with concrete activity

The composer status line above the input box shows the most specific live
activity instead of a generic busy label: a live workflow wave
(`<agent> tool · bash · <fragment>` from the supervisor feed), a delegated
subagent job (`kimi read tools.rs · 12s`), a running tool
(`read src/lib.rs`), `Compacting context…`, `thinking…`, then any concrete
status text and finally the `Working…` fallback — every source bounded and
credential-redacted, with a spinner, `⟦esc⟧` hint, and a reserved
gap·text·gap row layout mirroring OMP
(`crates/pi-cli/src/tui.rs:10517-10711,10824-10952`). Tests cover source
precedence, animation, idle blankness, and footer stability
(`tui.rs:26212-26268,28893-28931`).

### RESOLVED — mermaid over-budget always splits; CJK keyword boundary, zero-winsize divide, panic-hook ordering

The former 4× total output ceiling is removed: a flowchart or class diagram
beyond ANY total size now splits into bounded numbered panels instead of
erroring with `OutputLimit` and falling back to the raw source
(`crates/pi-coding/src/markdown/mermaid.rs:1988-2022`; 112-node and 500-node
goldens at `mermaid.rs:1887-1919`). `keyword_tail` now bounds its slice at a
char boundary, so a CJK statement whose length check falls inside a multibyte
char (e.g. `loop 每个模型回合` against the 10-byte `autonumber`) cannot panic
on a mid-char index (`mermaid.rs:996-999,1375-1379`). The TUI's
terminal-cell-size probe uses `then(|| …)` instead of `then_some(…)`: the
eager argument previously divided by zero on a zero-column pty (no winsize)
and panicked on the first draw (`crates/pi-cli/src/tui.rs:11429-11434`). The
process-wide panic hook prints the report to stderr BEFORE restoring the
terminal, because the restore emits a tmux passthrough DCS that leaves tmux's
parser unable to render anything written after it — a crash used to kill the
session with zero visible output (`tui.rs:1272-1295`; installed before any
dispatch at `crates/pi-cli/src/main.rs:18-22`).

### RESOLVED — content-anchored transcript window

The scroll-up window is anchored to a transcript entry plus a row offset
inside it instead of to the bottom: when content below the window changes
height — streaming growth, a mermaid fence closing (source↔diagram settle
flip, which can move tens of rows), or a resize reflow — the re-anchored
window keeps showing the same content instead of drifting by the height delta
(`crates/pi-cli/src/tui.rs:2636-2648,15101-15147`). The regression drives a
mermaid block that grows between frames and asserts the anchor, top row, and
entry survive, plus resize re-anchoring at 40/80/100 columns
(`tui.rs:19925-20033`).

### RESOLVED — CJK cursor tracking via WideCellCrosstermBackend

Ratatui 0.29 tracks the previous update's buffer coordinate rather than the
physical terminal cursor; for adjacent width-2 CJK glyphs that emits a
`MoveTo` before every glyph, reproducing the reported inter-character
spacing. The TUI keeps the upstream backend for non-draw operations but
renders diffs through `WideCellCrosstermBackend`, which advances the cursor
by the last symbol's physical display width (`.max(1)`)
(`crates/pi-cli/src/tui.rs:1318-1328,1421-1425`).

### RESOLVED — markdown/TUI polish batch

Task-list items render distinct checkbox glyphs (`☐`/`☑`) in both renderers,
never literal `[ ]`/`[x]` text, including empty-body, multi-space, nested,
and wrapped cases (`crates/pi-coding/src/markdown/render.rs:1320-1400`).
Markdown tables frame with the same box chrome AND the card-border theme role
as tool cards (`crates/pi-cli/src/tui.rs:14368`; regression at
`tui.rs:16859-16861`). Code-review comment cards render complete green frames
(labeled top, side borders, closing bottom) on the user surface, and the
"Review thread" summary row is removed
(`crates/pi-cli/src/code_review_panel.rs:1852-1856,2253-2257,2970-3027,
3975-4025`; no-summary-row assertions at `code_review_panel.rs:3455,3504`).
Read cards show a single fold footer as the only "more lines" notice — the
tool's file-level offset hint never renders (`tui.rs:17387-17420`). Thinking
blocks render without a standalone "thinking ·"/"Reasoning" label
(`tui.rs:17021-17022,19747-19749`). User-card padding rows are the single
styled blank between entries, and the TodoHUD strip reserves one leading and
one trailing blank row so it never renders flush against adjacent content
(`tui.rs:12416-12420,12743-12746`).

## Documentation

### RESOLVED — mdbook documentation

rpi ships an mdbook site: `docs/book.toml` with `docs/src/SUMMARY.md`
covering the introduction (install/quickstart), user-guide chapters
(cli-modes, tui, goals, todos, workflows, orchestration, rpc-json, live,
session-recovery, e2e-scenarios, web, models, authentication), and reference
chapters (architecture, tools, memory, extensions, security, packages,
settings-trust, configuration-profiles, hooks, acp, mcp, sandbox-isolation,
skills, prompt-templates, export-share, environment-variables, update,
local-llama). The web client's deployment, auth, and panel surface is
documented in `docs/src/user-guide/web.md`; `docs/src/README.md` explains the
book layout and how chapters map to source areas.

## Priority roadmap summary

Ranked gaps against OMP/grok by impact and implementation effort. Details,
peer evidence, and risk levels are in the sections above.

| Gap | Impact | Effort |
|---|---|---|
| **Security permissions/hardening** | Filesystem sandbox resolved; OS isolation for subagents/trusted extensions remains | Medium |
| **Secrets/session retention** | Presentation redaction, rewind/checkpoint, handoff, and snapcompact resolved; raw secrets still stored verbatim in session JSONL | Medium |
| **Web search** | Resolved: DuckDuckGo Instant Answer tool now available | — |
| **Document reading** | Resolved: PDF, Office, EPUB, and Jupyter notebook conversion now available | — |
| **Browser** | Resolved: headless Chrome/Chromium `browser` tool now available | — |
| **LSP** | Resolved: `lsp` tool with 10 actions now available | — |
| **MCP** | Resolved: MCP client + `mcp` tool now available | — |
| **Memory** | Resolved: per-project `memory` tool now available | — |
| **Subagent visibility** | Resolved: live per-subagent progress, activity feed with outcomes, and `history://` transcript hint in the TUI | — |
| **OAuth** | Missing `github-copilot`/`radius` providers; Openrouter refresh and Anthropic flow differ | Medium |
| **Hooks** | Resolved: host-level hooks now implemented for session/turn/tool-call events | — |
| **Web UI** | Resolved: `rpi --listen` serves an embedded React client (todo/goal/workflow/session/settings/subagents/side-chat/maintenance panels, markdown+mermaid+KaTeX renderer, 10 playwright e2e lanes) | — |
| **Collab** | Resolved: encrypted live collaboration protocol, `/collab`/`/join`/`/leave` interactive commands, privacy-redacted server-side snapshots, and browser guest join route are all implemented; full E2E scenario passes (control 35/35, view 30/30, browser 13/13) | — |
| **ACP** | Resolved: ACP v1 (`rpi agent stdio`/`serve`) now available | — |
| **Desktop automation (`computer`)** | Cannot drive the host desktop (screenshots, clicks, typing, scrolling) | High |

Approval modes are now the safety baseline. Suggested next order: OS isolation
for subagents and trusted extensions → secrets/session retention → OAuth →
lower-impact ecosystem parity (desktop `computer` automation, highest effort
last).

## Internal code quality: file granularity

Focused panels and control-plane code now live in separate modules, but the
central runtime files remain far above the 800-line investigation threshold.
Current counts from the working tree are:

| File | Lines | Over 800-line threshold |
|---|---:|---:|
| `crates/pi-cli/src/tui.rs` | 32,049 | 40.1× |
| `crates/pi-coding/src/session.rs` | 8,859 | 11.1× |
| `crates/pi-coding/src/orchestration/runtime.rs` | 8,447 | 10.6× |
| `crates/pi-cli/src/modes/rpc.rs` | 5,716 | 7.1× |
| `crates/pi-coding/src/tools.rs` | 5,530 | 6.9× |
| `crates/pi-coding/src/extensions.rs` | 5,358 | 6.7× |
| `crates/pi-coding/src/application.rs` | 4,502 | 5.6× |
| `crates/pi-coding/src/session_store.rs` | 4,477 | 5.6× |

The largest files are also the highest-risk state machines. Mechanical splits
while durable recovery, live control, and side-chat behavior are changing would
hide regressions rather than reduce risk. New features therefore use focused
modules (`modes/listen.rs`, `settings_panel.rs`, `todo_dag_panel.rs`,
`code_review.rs`, `code_review_panel.rs`, and side-chat modules), with central
files limited to wiring.

Refactoring order after behavior is frozen and E2E-protected:

1. split orchestration runtime/persistence/job lifecycle by ownership and
   durability boundaries;
2. split session storage/compaction/runtime behavior without moving public
   wire types;
3. split TUI state, event routing, panels, and rendering while keeping one
   overlay arbiter and terminal lifecycle owner; and
4. split RPC command families around the shared dispatcher.

Each extraction must be behavior-preserving and independently verified; no
style-only mass move should be mixed with a feature change.

## Resolution snapshot

The current inventory contains 131 classified entries: 103 `RESOLVED`, 28 `By
design`, and 0 `PARTIAL`. Observable fixes
therefore account for 78.6% of all entries. Excluding deliberate divergences,
all 103 actionable entries are resolved (100%). These
figures describe this document's headings, not test coverage or release
readiness; the evidence behind each `RESOLVED` entry must be refreshed by
running the commands in the Verification checklist before a release.

## Verification

Compatibility claims cite the current working-tree implementation and the
pinned peer/upstream snapshots named in their sections. `RESOLVED` means an
observable regression or gap was fixed during this implementation wave.

**Verification:** Before a release is declared, the following commands
must be executed and their results inspected; this paragraph is a checklist,
not a pass report.

- `cargo +1.88.0 check --workspace --all-targets --locked`
- `cargo +1.88.0 test -p pi-ai --lib --locked`
- `cargo +1.88.0 test -p pi-ai --test codex_transport --locked -- --test-threads=1`
- `cargo +1.88.0 test -p pi-agent --lib --locked -- --test-threads=1`
- `cargo +1.88.0 test -p pi-coding --lib --locked -- --test-threads=1`
- `cargo +1.88.0 test -p pi-cli --lib --locked -- --test-threads=1`
- `cargo +1.88.0 llvm-cov --workspace --locked --json --output-path target/pi-rs-cov-final.json -- --test-threads=1`
- `cargo +1.88.0 test -p pi-coding --lib sandbox --locked`
- `cargo +1.88.0 test -p pi-sandbox --all-features --locked -- --test-threads=1`
- `cargo +1.88.0 test -p pi-coding --test persona_e2e --locked`
- `cargo +1.88.0 test -p pi-cli --test persona_cli_e2e --locked`
- `cargo +1.88.0 test -p pi-coding --lib hindsight --locked`
- `RPI_BIN=target/release-dist/rpi bash E2E.d/release/install-self-update.sh run`
- `RPI_BIN=target/release-dist/rpi bash E2E.d/release/archive-fixture-smoke.sh run`
- `RPI_BIN=target/release-dist/rpi bash E2E.d/ci/workflow.sh run`
  — evidence under `$EVIDENCE_ROOT/workflow`
- `RPI_BIN=target/release-dist/rpi bash E2E.d/collab/collab_scenario.sh run`
  — evidence under `$EVIDENCE_ROOT/collab`
- `bash E2E.d/web/coverage.sh run` — measured gate that runs every web lane:
  `core goal xss abort reconnect switch mobile auth extras sessions`
- `target/release-dist/rpi --version` must print `rpi 0.2.4`
- `git diff --check` after staging the release artifacts
