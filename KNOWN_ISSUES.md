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

### LOW — rpi-only `todo_snapshot` entry type

rpi appends `"type":"todo_snapshot"` records; upstream's `parseSessionEntries`
skips unknown types, so Todo state does not survive a round-trip through
upstream. No data loss, but no interoperability either.

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

### LOW — bash tool env vars compatible

The `PI_SESSION_ID`, `PI_SESSION_FILE`, `PI_PROVIDER`, `PI_MODEL`, and
`PI_REASONING_LEVEL` variables exported to bash tool children match upstream.

### By design — rpi-only variables

`PI_HOME`, `PI_FAUX_RESPONSE`, `PI_CACHE_RETENTION`, `PI_OAUTH_CALLBACK_HOST`,
`PI_SHARE_VIEWER_URL`, `PI_BUN_EXECUTABLE`, `PI_CLIPBOARD_IMAGE`,
`PI_SKIP_VERSION_CHECK` are rpi extensions. They do not affect compatibility.

## Resources (skills, prompts, themes, keybindings)

### RESOLVED — `.agents/skills/` roots are supported with trust gating

rpi discovers the platform user `.agents/skills/` root and trusted project or
ancestor `.agents/skills/` roots through typed resource roots. Candidate roots
are classified from their canonical targets: project-bound targets are excluded
when untrusted and forced to project scope when trusted, including global-path
symlink aliases. `.pi/skills/` retains the highest precedence
(`crates/pi-coding/src/resource_manager.rs`; focused resource-manager tests).

### MEDIUM — keybinding default chords differ

Upstream's `KEYBINDINGS` object and rpi's `default_bindings()` map the same
action ids to different default chords in places — e.g. `ctrl+u` is `app.clear`
upstream but `EditorClear` in rpi
(`packages/coding-agent/src/core/keybindings.ts:12-55` vs
`crates/pi-cli/src/keybindings.rs:655-730`). Users switching between the two
CLIs experience different default shortcuts.

### RESOLVED — later-loaded prompt templates win name conflicts

Prompt templates now use later-loaded-wins precedence: global, then project,
then explicit sources, with the winning template retaining its later position
(`crates/pi-coding/src/prompt_templates.rs:172-192`). Collision/order behavior
is locked by `later_duplicate_shadows_earlier_at_later_position`
(`prompt_templates.rs:428-444`).

### MEDIUM — skill `<location>` uses `skill://` URI, upstream uses absolute paths

rpi's skill prompt lists locations as `skill://<name>` URIs
(`crates/pi-coding/src/resources.rs:1358-1390`); upstream lists absolute file
paths (`packages/coding-agent/src/core/skills.ts:335-365`). Models tuned on
upstream's absolute-path format may mishandle the URI scheme, and extensions
that parse the location field see different values.

### LOW — rpi-only skill frontmatter fields

rpi additionally parses `globs`, `alwaysApply`, and `hide`/`hidden` from skill
frontmatter (`crates/pi-coding/src/resources.rs:660-673`); upstream ignores
unknown frontmatter. Upstream skills load unchanged; the extra fields are
rpi extensions for the selector.

### LOW — theme `extends` and strict unknown-field rejection

rpi themes support an `extends: "dark"|"light"` base
(`crates/pi-cli/src/theme.rs:503-509`) and reject unknown fields
(`theme.rs:485,478`); upstream hardcodes two themes and is lenient. Valid
upstream theme JSON loads fine; the differences are rpi-only features.

### LOW — project-level keybindings file

rpi additionally loads `.pi/keybindings.json` (project-scoped) and applies
strict validation (`crates/pi-cli/src/keybindings.rs:812-870`); upstream only
reads `~/.pi/agent/keybindings.json`. Compatible for upstream-authored files.

### MEDIUM — read tool lacks PDF/Office/notebook conversion

rpi's `read` tool supports text files and images (jpg, png, gif, webp, bmp)
only (`crates/pi-coding/src/tools.rs:628`); there is no document conversion
for PDF, PowerPoint, Word, Excel, RTF, EPUB, or Jupyter notebooks. A model
pointed at a `.pdf` or `.pptx` gets raw binary/decoding noise instead of
extracted text.

Peer contrast:

- **OMP** `read` converts `.pdf .doc .docx .ppt .pptx .xls .xlsx .rtf .epub`
  via markit to text/markdown, supports line-range selectors on the converted
  output (`file.pdf:50-100`), extracts embedded PDF images as browsable
  handles (`doc.pdf:p11-img0.png`), renders `.ipynb` as editable
  `# %% [...] cell:N` text, and handles URL binary fetches of
  image/PDF/DOCX (`tools/read.md:171-234`).
- **grok-build** `read_file` handles text, PDF, PowerPoint, notebooks, and
  images through format-specific paths
  (`crates/codegen/xai-grok-tools/src/implementations/grok_build/read_file/mod.rs:45-120,420-535`).

**Impact:** rpi cannot answer questions about documents (specs, slides,
notebooks) natively; users must pre-convert or prompt around the binary
payload. This also blocks PDF-driven workflows that OMP/grok handle.

**Fix direction:** add document conversion to the `read` tool, ideally by
shelling out to a converter (markit or pdftotext/antiword-style tools) with
bounded output, or by porting the conversions in Rust. Priority: PDF (most
common) → PowerPoint → notebook (.ipynb editable text) → Office
(doc/docx/xls/xlsx). Keep the existing image path; add PDF image extraction
as a follow-up if the converter surfaces embedded images.

### MEDIUM — no web search tool

rpi has no `web_search` tool. A grep across all crates finds zero matches
for `web_search`, `web-search`, `webfetch`, or `web_fetch`. The model cannot
fetch live web content during a session.

Peer contrast:

- **OMP** ships `web_search` as a built-in tool with configurable search
  provider, recency filters, and result limits
  (`packages/coding-agent/src/tools/web-search.ts`).
- **grok-build** gates web search behind `[features] web_fetch` and
  integrates it into the tool dispatch
  (`crates/codegen/xai-grok-config/src/features.rs:1-50`).
- **upstream pi** includes `web_search` as a core tool
  (`packages/coding-agent/src/tools/web-search.ts`).

**Impact:** rpi cannot answer questions about current events, docs, or
live data; users must paste content manually or switch to another agent.

**Fix direction:** add a `web_search` tool that shells out to a search API
(SerpAPI, Bing, or a local scraper) with configurable provider, result
count, and recency. Gate it behind a feature flag or capability declaration
for security-conscious deployments.

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

### HIGH — `npm:` package source rejected

Upstream treats `npm:` as a first-class package source with version ranges and
pin resolution (`packages/coding-agent/src/core/package-manager.ts:142`); rpi
explicitly rejects it (`crates/pi-coding/src/packages.rs:628-631`).
`ManagedPackageSourceKind` only has `Local` and `Git` (`packages.rs:70`).
Configured `npm:` packages are listed but never contribute resources
(`packages.rs:843-846`, `:871-875`, `:926-930`).

### MEDIUM — OAuth provider set differs

Upstream supports `github-copilot` and `radius` OAuth
(`packages/ai/src/auth/oauth/`); rpi does not
(`crates/pi-coding/src/oauth.rs`). rpi additionally supports
`google-gemini-cli` OAuth, which upstream lacks. Anthropic OAuth flow differs:
upstream uses a callback server on port 53692; rpi uses manual code paste.
Openrouter OAuth in rpi has no refresh-token handling. Users of the missing
providers cannot authenticate on rpi.

### MEDIUM — HTML export rendering differs

Upstream export uses `marked.js` + `highlight.js` and a custom TUI-to-ANSI-to-
HTML pipeline for tool entries (`packages/coding-agent/src/core/export-html/`);
rpi uses its own markdown renderer with no code highlighting and no custom
tool rendering (`crates/pi-coding/src/export/mod.rs`). Exported HTML from the
two CLIs differs in fidelity.

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

### MEDIUM — system-bash spawn instead of embedded brush shell

rpi executes every bash tool call by spawning a system shell:
`/bin/bash` → `bash` on PATH → `sh` (`crates/pi-coding/src/tools.rs:1141-1149`),
then `Command::new(shell).arg("-c").arg(command)` per invocation
(`tools.rs:1273-1275`). This matches upstream `pi`, but it has three costs:

1. **Windows requires Git Bash.** On Windows there is no POSIX shell by
default; upstream `pi` probes known Git Bash locations then fails with a
setup hint (`packages/coding-agent/src/utils/shell.ts:62-107`). rpi inherits
this dependency.
2. **Per-command spawn overhead.** Each call pays process spawn
(~5–10 ms), negligible next to model latency but not free.
3. **Profile loading injects environment.** Spawned bash loads the user's
`.bashrc`, so command behavior depends on machine-local aliases, functions,
and exported variables the model cannot see.

Peer contrast: OMP replaces the system shell with an **embedded brush shell**
— a Rust bash-compatible engine (~34k lines in `pi-shell`) that runs
in-process, skips profile/rc loading, disables `exec`/`suspend`, registers
native `sleep`/`timeout`/`nohup` builtins, and keeps behavior identical across
platforms (`crates/pi-shell` brush session; `crates/pi-uu-grep`/`pi-uu-diff`
as in-process builtins). grok-build keeps a persistent terminal backend per
session, preserving shell state across calls
(`crates/codegen/xai-grok-tools/src/implementations/grok_build/bash/mod.rs:1637-1638`).

**Decision:** rpi should adopt an embedded brush-style shell (port OMP's
`pi-shell` or vendor brush-core) to eliminate the Windows bash dependency,
remove per-command spawn cost, and make command behavior independent of the
user's `.bashrc`. This is a feature decision, not a bug fix; the current
system-bash path is fully functional and matches upstream.

## Session behavior

### HIGH — resumed model is restored by rpi but not by upstream

On `--resume` with no `--model`, rpi restores the session's recorded provider
and model (`crates/pi-cli/src/session_run.rs:307-560`); upstream ignores the
recorded model and always applies the settings/CLI default
(`packages/coding-agent/src/main.ts:420-482`). The two CLIs pick different
models for the same resumed session.

### MEDIUM — resumed thinking level differs

rpi restores the recorded thinking level on resume
(`crates/pi-cli/src/session_run.rs`); upstream does not
(`packages/coding-agent/src/main.ts:469-472`).

### RESOLVED — session lists use last-modified ordering

`list_sessions_in` orders files by descending filesystem mtime, then session
timestamp and path for a deterministic total order
(`crates/pi-coding/src/session_store.rs:1673-1710`). The regression fixture
asserts that a touched older session sorts ahead of a newer header timestamp
(`session_store.rs:3256-3290`).

### MEDIUM — no extension trust hook

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
(`crates/pi-cli/src/session_run.rs:307-310`); upstream picks any `.jsonl` in the
session directory. Because rpi imports multiple session formats, it must not
surprise-import a foreign session.

### RESOLVED — durable child recovery and mailbox-triggered revival

The orchestration runtime now recovers existing sidecar state before any write.
`bind_and_recover` loads the prior `orchestration-state.json` and prepares the
recovered agent/job snapshots before committing the durable binding; only when
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

### PARTIAL — subagent autonomy also lacks soft budgets and yield-driving

Independent of durability, rpi still lacks two peer controls: a soft request
budget with steering/hard ceiling, and a yield-driving protocol that reminds a
child to finish and can force a final yield attempt.

### PARTIAL — sibling roster is injected; no dedicated `ask` tool

Subagents now receive a bounded, XML-safe spawn-time `<peer_roster>` containing
Main and live siblings. Batch spawn first registers every child, snapshots all
rosters, then launches, so same-batch siblings see each other
(`crates/pi-coding/src/orchestration/runtime.rs:1899-1988,2395-2445`). The
roster is capped at 64 entries / 16 KiB and keeps Main when truncating.

The remaining gap is discoverability of request/response messaging: OMP has a
dedicated `ask` tool, while rpi still uses `hub send` with `await_reply`. The
generic path is fully functional but requires the model to construct the flow.

### MEDIUM — subagent activity is invisible mid-run; no live progress in the TUI

rpi's `subagentContainer`-equivalent (the job-card projection) shows only an
agent name and coarse queued/running state — not what the subagent is doing
right now. There is no live per-subagent progress surface showing the current
tool, stage, or token usage while it runs, and no way to open the subagent's
session transcript in the TUI (`crates/pi-cli/src/job_card_adapter.rs`, the
`job_cards` projection in `tui.rs`). A user watching a long `task` batch has no
visibility into any individual subagent until it finishes and its
`agent://<id>` artifact exists.

Peer contrast:

- **grok-build** opens a fullscreen framed subagent view on click: title bar
  with status icon/label/model/elapsed, the child's own scrollback, thinking,
  tool calls, and a prompt area (`docs/user-guide/16-subagents.md:309-315`).
- **OMP** streams live progress into the task tool block (`onUpdate`, 150ms
  coalesce, `tools/task.md:59`) and exposes `task:subagent:progress` events
  plus `set_subagent_subscription` (RPC: `off|progress|events`,
  `subsystem-prompt-config.md:2052`), but its `subagentContainer` also shows
  only names — neither surfaces a per-subagent live view in the TUI.

**Impact:** when spawning several subagents, the user "has no idea what they're
doing" until results land; long-running work is a black box.

**Fix direction:** extend the existing job-card projection to render live
per-subagent state (current tool, stage, token usage, elapsed) from the
`OrchestrationEvent` stream (already emitted by `runtime.rs`), and add a
subagent detail view that opens the child transcript from `history://<id>` on
the existing panel framework (`TreePanel`/`WorkflowPanel`) — mirroring grok's
framed subagent view and OMP's `history://` transcript rendering.

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

### PARTIAL — settings editing is typed; section hierarchy remains flat

The settings panel now derives native controls from `SettingValueType`:
boolean and enum controls, bounded numeric inputs, strings, lossless JSON
string arrays, generic lists/objects, and non-readable secret state
(`crates/pi-cli/src/settings_panel.rs:25-55,454-508`). Paste is routed to the
active settings editor before the hidden composer, invalid values stay open,
and Escape backs out one level at a time (`crates/pi-cli/src/tui.rs`).

The remaining gap versus OMP is navigation, not typing: categories are still a
filter over one row list rather than a first-class section→field hierarchy,
and global/project scope is not presented as a persistent visible tab. A future
panel-only refactor can address that without changing the typed controller.


### PARTIAL — composer chrome shows model/thinking/cwd/git/context/status; cost and task counts remain absent

The composer header now renders the active model, compact thinking level, cwd,
git branch/dirty counts, context-window utilization, and bounded transient
status/activity in the top border (`crates/pi-cli/src/tui.rs:7872-7970`). Git
metadata is collected off the render path by one bounded, generation-aware
background worker (`tui.rs:403-537,3820-3905`). Each refresh discovers the
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

The remaining status-line gaps are session cost/usage totals and a background
task count indicator; adding those fields requires an application-state
projection, but no longer requires redesigning the composer layout.

## CLI surface

### HIGH — `--export` interface differs

Upstream exports via the top-level flag `--export <file>`
(`packages/coding-agent/src/cli/args.ts:151`); rpi exposes the `export
<session_path>` subcommand. Scripts invoking `pi --export session.jsonl` fail
on rpi.

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
long option (`crates/pi-cli/src/args.rs:51-53`). The parser regression verifies
the alias and conflict behavior (`args.rs:622-629`).

### By design — no TUI flags

`--ui-mode` and `--alt` (alternate screen) are absent because rpi's TUI is a
normal-screen inline interface. Passing them to `rpi` is an unknown-flag error.

### By design — rpi-only flags and subcommands

`--cwd`/`-C`, `--add-dir`, and the `login`/`logout`/`models`/`config`/
`workflow` subcommand family are rpi extensions.

## RPC protocol

### RESOLVED — `get_commands` projects the primary command catalog

RPC discovery now returns the same ordered 14-command primary catalog used by
the interactive command UI, including `code-review` and `btw`, via
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

### LOW — response shapes are supersets

`get_state` adds `todoPhases`, `goal`, `runtimeSettings`; `get_tree` adds
`activeLeafId`; `set_thinking_level` returns a richer `{requested,level,
clamped,message}` payload. Strict-schema clients reject the extra fields;
lenient clients are unaffected. All 32 upstream commands are implemented, plus
40 rpi-only commands (`set_todos`, `loop_*`, `process_*`, `goal_*`,
`settings_*`, `workflow_*`).

### RESOLVED — `--listen` control plane with owned local UI and bounded pre-auth surface

Opt-in `--listen` starts a control plane around the same live `Application`
used by the text TUI/REPL. HTTP `POST /rpc` and WebSocket `/ws` share
`RpcDispatcher`, so commands target the on-screen session rather than a second
`rpi-rpc` process (`crates/pi-cli/src/lib.rs`; `modes/listen.rs`).

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

### LOW — `openrouter-images` API missing

Upstream defines `KnownImagesApi = "openrouter-images"`
(`packages/ai/src/types.ts:30-32`) with a full image-generation surface. rpi
has no images API; models configured with this API cannot load.

### LOW — OpenAI Responses API stateful chaining not implemented

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

The extension model is an architectural divergence, not a bug.

### HIGH — manifest format incompatible

Upstream declares extensions inline in `package.json#pi.extensions` pointing at
`.ts`/`.js` files (`packages/coding-agent/src/core/pi-manifest.ts:16-35`); rpi
requires a standalone `pi-extension.json` with `id`, `runtime`
(`"process"`/`"bun"`), `capabilities`, and `ui_capabilities`
(`crates/pi-coding/src/extensions.rs:184-320`). Every upstream extension needs
a new manifest to run on rpi.

### HIGH — process model differs

Upstream loads extensions in-process via jiti and allows importing
`@earendil-works/pi-*` internals (`packages/coding-agent/src/core/extensions/loader.ts:400-500`);
rpi runs each extension in a child process speaking JSONL
(`crates/pi-coding/src/extensions.rs:1300-1450`). Extensions depending on
upstream internals cannot run on rpi.

### HIGH — extensions cannot mount arbitrary in-process TUI components; rich host-mediated UI is supported

Upstream extensions are in-process, so they can mount their own components
into the TUI: `TUI.showOverlay()`, `Editor`/`Component`/`Focusable` from
`pi-tui`, direct `Agent`/`SessionManager` access, and prototype patching of
`ExtensionRunner.getAllRegisteredTools()` to capture every registered tool.
This is how the [`pi-side-chat`](https://github.com/nicobailon/pi-side-chat)
extension builds a real second agent in a TUI overlay: it forks the main
conversation via `buildSessionContext`, creates an independent in-process
`Agent` with all extension tools, renders it through `TUI.showOverlay()`, and
reads the main session live via a `peek_main` tool that calls
`sessionManager.getEntries()` (`side-chat-overlay.ts:1-133`,
`index.ts:20-26`). OMP's `ctx.ui.setEditorComponent()` /
`addAutocompleteProvider()` are the same in-process pattern
(`porting-from-pi-mono.md:309-310`).

rpi extensions are child processes speaking JSONL
(`crates/pi-coding/src/extensions.rs:1300-1450`), so they cannot access the
Rust `TuiState`/ratatui renderer, the `Application`/`Session`, or any other
in-memory host object, and they cannot inject arbitrary components or overlays.
What they *can* do is request rich, host-controlled UI operations through the
`ui_capabilities` protocol. The supported request surface includes interactive
widgets (`select`, `input`, `confirm`, `editor`), host chrome updates
(`status`, `widget`, `title`, `notify`), editor integration
(`setEditorText`, `getEditorText`, `pasteToEditor`), working-state indicators
(`setWorkingMessage`, `setWorkingVisible`, `setWorkingIndicator`), theme
queries/changes (`getAllThemes`, `getTheme`, `setTheme`), and tool-expansion
state (`getToolsExpanded`, `setToolsExpanded`)
(`crates/pi-coding/src/extensions.rs:930-1080`). The TUI implements the
interactive owner (`ExtensionUiAdapter`) and non-interactive observers with
per-extension cleanup (`crates/pi-cli/src/extension_ui.rs:154-225,446-558`).

The side-chat use case is now implemented as the built-in `/btw` overlay rather
than weakening this boundary. It owns a detached `Agent`, independent
transcript/editor/stream, read-only default tools, optional edit/exec mode, and
`peek_main` (`crates/pi-cli/src/side_chat.rs:1-5,149-269`). Its public E2E suite
verifies main-session isolation, abort, reopen persistence, cleanup, and tool
capabilities (`crates/pi-cli/tests/side_chat_e2e.rs`). This resolves the product
capability but not arbitrary extension component injection.

**Impact:** third-party extensions that require custom Ratatui components or
direct host-object access still cannot be ported as process extensions. They
must use host-mediated UI or be implemented as focused built-in Rust features.
A general remote-component protocol remains an architecture decision.

### MEDIUM — no general multi-tab session container; `/btw` provides one persistent parallel conversation

rpi still has no N-tab session container, tab bar, or arbitrary background
session switching. `SavedSessionSelector` and `Application::switch_session`
replace the active main session rather than preserving several main tabs.

The high-value side-conversation case is implemented: `/btw` forks the active
branch into a detached Agent, persists its controller while the overlay is
closed, streams independently, defaults to read-only tools, can explicitly
toggle edit/exec tools, and exposes `peek_main`
(`crates/pi-cli/src/side_chat.rs:149-269`). The overlay has independent input
ownership and shutdown/abort/refork/clear lifecycle; the public tests verify
that side-chat activity does not mutate the main transcript or stream state
(`crates/pi-cli/tests/side_chat_e2e.rs:468-581`).

The remaining gap is generality: users cannot keep N full main sessions alive
as switchable tabs. Implementing that requires a TUI-level container owning
multiple `Application` instances and per-tab event/render/input state.


### LOW — no `/live` realtime voice support

rpi has no voice or realtime-audio capability: no speech-to-text, no
voice-stream input, no `/live`-style continuous voice conversation. A grep
across all crates finds only image-protocol detection
(`crates/pi-cli/src/terminal_images.rs:72-76`) and PTY support
(`crates/pi-coding/src/process/`); there is no audio/voice/realtime module.

Peer contrast:

- **codex** supports realtime voice sessions: `RealtimeConversationStart`
  (`codex-rs/protocol/src/protocol.rs:532`), `RealtimeConversationStarted`
  event (`:1296`), and a `<realtime_conversation>` context fragment
  (`:352`).
- **grok-build** ships `xai-grok-voice` (speech-to-text capture) and biases
  the event loop so voice STT runs only when higher-priority arms are idle
  (`crates/codegen/xai-grok-pager/Cargo.toml:1-160`,
  `event_loop.rs:1819-1828`), gated by `[features] voice_mode`.

**Impact:** hands-free interaction and realtime voice conversations are
unavailable; users must type.

**Fix direction:** this is a feature decision, not a bug fix. A minimal
`/live` would need: an audio capture backend (platform mic), streaming STT
(provider or local model), voice-stream injection into the existing
`Application.prompt` path, and TUI affordances for voice state. The event-loop
biasing lesson from grok (voice last, never starve cancellation/input)
applies if implemented.

### HIGH — capability declarations required

rpi requires explicit `capabilities` / `ui_capabilities` declarations and
rejects undeclared calls; upstream has no permission model. Extensions must
declare what they use.

### HIGH — `registerProvider` / `unregisterProvider` unsupported

Extensions that register or override LLM providers cannot run on rpi (they
fail closed via `unavailable()` in the Bun host).

### MEDIUM — rendering and session-control APIs unsupported

`registerMessageRenderer`, `registerMarkdownTransformer`,
`registerEntryRenderer`, `exec`, `newSession`, `fork`, `navigateTree`,
`switchSession`, `modelRegistry`, `scopedModels`, and several `ui.*` methods
(`onTerminalInput`, `setFooter`, `setHeader`, `custom`,
`addAutocompleteProvider`, `setEditorComponent`) are unavailable.

### MEDIUM — tool/command definitions are a subset

`renderCall`, `renderResult`, `renderShell`, `prepareArguments`,
`constrainedSampling` on tools, and `getArgumentCompletions` on commands are
rejected (`crates/pi-coding/src/bun_extension_host.mjs:210-230`).

### LOW — events are fully compatible

All 33 upstream extension events exist in rpi's Bun host
(`crates/pi-coding/src/bun_extension_host.mjs:30-40`) with matching names and
payload shapes. Simple event-listener / tool / command / shortcut extensions
port with a new manifest and without the unsupported APIs.

## Sandbox and process isolation

rpi has **no OS-level sandbox** for tool execution. This is inherited from
upstream `pi` (which also has none), but it is a real gap versus peer agents
that ship one. The comparison targets are [`openai/codex`](https://github.com/openai/codex)
and [`xai-org/grok-build`](https://github.com/xai-org/grok-build), not upstream
`pi`.

### HIGH — no filesystem sandbox

Every tool (read, edit, write, bash, grep, find) runs with the full privileges
of the `rpi` process. There is no bubblewrap/Landlock (Linux), Seatbelt (macOS),
or Restricted Token (Windows) layer confining a child command's filesystem
visibility. A malicious or buggy `bash` call can read or write any path the
user can (`crates/pi-coding/src/tools/bash.rs`, `crates/pi-coding/src/tools.rs`).

Peer contrast: Codex wraps tool execution in `codex-linux-sandbox`
(bubblewrap + seccomp) / Seatbelt / Windows Restricted Token
(`codex-rs/sandboxing/src/manager.rs:29`, `codex-rs/linux-sandbox/src/linux_run_main.rs`);
grok-build applies Landlock/Seatbelt via `nono`
(`crates/codegen/xai-grok-sandbox/src/lib.rs:1-214`).

### HIGH — no path-level permission rules

There is no settings surface equivalent to grok-build's `[permission]
allow/deny/ask/rules` or Codex's `SandboxPolicy` (`DangerFullAccess`,
`ReadOnly { network_access }`) / `PermissionProfile`. The only trust control is
project-level trust (`crates/pi-coding/src/trust.rs:17-79`): either a project
directory is trusted wholesale or it is not. There is no way to express
"the agent may not read/write this specific directory" from settings.json.

Peer contrast: grok-build `[permission]` static rules
(`crates/codegen/xai-grok-workspace/src/permission/manager.rs:1210-1770`);
Codex `SandboxPolicy` (`codex-rs/protocol/src/protocol.rs:1014-1027`).

### HIGH — no process hardening

rpi does not disable core dumps, block ptrace attach, or strip `LD_`/`DYLD_`
environment variables at startup. A crash can leave a core dump containing
in-memory secrets; an attached debugger can inspect process memory. Codex runs
`pre_main_hardening` before `main` via `#[ctor::ctor]`
(`codex-rs/process-hardening/src/lib.rs:9-98`) for exactly this.

### MEDIUM — no subagent isolation beyond process boundaries

Extensions are isolated child processes (`crates/pi-coding/src/extensions.rs:1300-1536`)
and workflows use git worktrees (`crates/pi-coding/src/workflow_worktree/mod.rs:1-83`),
but these are crash/lifecycle isolation, not OS sandboxes. A trusted extension
or workflow child has full host privileges. This is documented in
`docs/security.md` as the intended trust model, so the gap is the absence of
an opt-in stronger boundary, not a silent weakening.

### By design — trust is the authorization boundary

Upstream `pi` explicitly treats the user account as the trust boundary
(`SECURITY.md`), and rpi inherits that stance: extensions, skills, and project
resources are either trusted or not loaded (`crates/pi-coding/src/trust.rs:17-79`).
Closing the sandbox gap is a feature decision (adopt bubblewrap/Seatbelt,
add `[permission]` rules), not a bug fix.

### RESOLVED — users can attach directly to a running PTY

The `/ps` process panel can attach only to a running `tty=true` process.
Printable input, Enter/navigation/control keys, and bracketed paste are written
directly through `ProcessManager`; `Esc`, `Ctrl+]`, and the legacy `Ctrl+5`
encoding detach locally (`crates/pi-cli/src/tui.rs:4580-4669,4875-4894`).
Process exit and I/O failure auto-detach.

This path bypasses the model tool call, composer, session messages, transcript,
status history, and structured stdout. Unit regressions verify direct routing,
detach precedence, bounded transient output, and no transcript echo
(`tui.rs:9143-9430`); the real PTY integration exercises attach, typing,
interrupt, and detach (`crates/pi-cli/tests/process_ps_pty.rs:439-524`).

## Missing tools (OMP/grok have, rpi does not)

rpi ships a lean core tool set: `read, bash, edit, write, grep, find, glob, ls,
todo, process, task, hub, goal`. OMP documents 31 tools; grok registers native
tools plus compatibility ports. The following are missing entirely from rpi
(verified by grep across `crates/`).

### MEDIUM — no browser/desktop automation tools

OMP ships `browser` (headless Chromium/CDP: tabs, DOM/ARIA snapshots,
click/type/fill, JS eval — `omp://tools/browser.md`) and `computer` (native
desktop capture and input: screenshots, clicks, typing, scrolling —
`omp://computer-use.md`). rpi has neither. A model cannot inspect a web page
or drive the host desktop.

**Fix direction:** add a `browser` tool (CDP over a spawned Chromium, or an
embedded headless engine) before considering `computer`; the latter needs
platform-native capture/input code.

### MEDIUM — no image generation or inspection tools

OMP ships `generate_image` (6 providers: OpenAI/Antigravity/OpenRouter/xAI/
Gemini — `omp://tools/generate_image.md`) and `inspect_image` (vision-model
analysis of a local image, auto-resize, WebP exclusion —
`omp://tools/inspect_image.md`). Grok ships image generation/editing and
video generation tools with byte/dimension/storage budgets
(`analysis.md:1681-1683`, feature gates `image_gen`/`video_gen` at `:819`).
rpi has neither. The existing `image_pipeline.rs` (resize/encode for inline
images) is input-only; there is no generation and no vision analysis tool.

**Impact:** rpi cannot create images, cannot describe screenshots beyond raw
pixel blocks, and cannot answer "what is in this image" through a dedicated
path.

**Fix direction:** `inspect_image` is the smaller win (reuse existing
vision-capable models via a new tool); `generate_image` needs provider APIs
and artifact storage.

### MEDIUM — no AST tools

OMP ships `ast_grep` (tree-sitter structured search with `$NAME`/`$_`/
`$$$NAME` metavariables — `omp://tools/ast-grep.md`) and `ast_edit` (pattern
rewrites with preview and explicit resolve — `omp://tools/ast-edit.md`). rpi
has none; the harness's own `ast_edit` device is not exposed to the session
model.

**Fix direction:** add `ast_grep`/`ast_edit` tools backed by the `ast-grep`
crate, reusing the harness tool contracts.

### MEDIUM — no code-intelligence tools (LSP, codebase index)

OMP ships `lsp` (14 actions: diagnostics/definition/references/rename/symbols/
code_actions/capabilities — `omp://tools/lsp.md`) and grok ships LSP tools
gated by `[features] lsp_tools` (`analysis.md:1001-1006,873`) plus a
tree-sitter codebase index (`xai-codebase-graph`, go-to-definition/references,
`analysis.md:2093-2222`). rpi has neither; `crates/pi-coding/src/tools.rs:3207`
explicitly rejects the `lsp` tool name. The harness's own `lsp` device is not
exposed to the session model.

**Impact:** the model relies on `grep`/`glob` for navigation; cross-file
renames and symbol lookups that a language server resolves precisely are done
by textual search.

**Fix direction:** expose an `lsp` tool that spawns language servers
(rust-analyzer etc.) per workspace; codebase indexing is a larger project.

### MEDIUM — no GitHub tool

OMP ships `github` (gh CLI wrapper: repo_view, pr_create/checkout/push,
issue/PR/code/commit/repo search, Actions run_watch — `omp://tools/github.md`).
rpi has no GitHub integration; PR workflows must shell out manually.

### MEDIUM — no debug, eval, or notebook tools

OMP ships `debug` (DAP sessions: launch/attach, breakpoints, evaluate,
stack traces, memory read/write — `omp://tools/debug.md`), `eval` (persistent
Python/JS cell runtime with cross-cell state — `omp://tools/eval.md`),
`checkpoint`/`rewind` (mark state, collapse exploratory context —
`omp://tools/checkpoint.md`), and `security_scan` (software security review
pipeline — `omp://tools/security_scan.md`). rpi has none of these.

### MEDIUM — no memory system

OMP ships an autonomous memory backend (`memory.backend: local` extraction →
integration pipeline, `memory://` URLs, `/memory` — `omp://memory.md`) with
four tools: `learn`, `recall`, `reflect`, `retain` (`omp://tools/learn.md`,
`recall.md`, `reflect.md`, `retain.md`), plus `memory_edit` and
`manage_skill` (`omp://tools/memory_edit.md`, `manage_skill.md`). Grok ships
cross-session memory (`[memory]` enabled/index/embedding/search,
`grok memory` command — `analysis.md:968-972,853,1050`). rpi has no
cross-session memory; skills are static files.

### LOW — no skill management tools

OMP ships `manage_skill` (create/update/delete managed skills under
`~/.omp/agent/managed-skills/` — `omp://tools/manage_skill.md`). rpi's
`/skills` command lists/reads skills but cannot author them from the session.

## Missing workflow/interaction modes

### RESOLVED — approval modes enforce typed read/write/exec capabilities

rpi exposes `yolo`, `write`, and `ask` through `--approval-mode` and the
global-only `approvalMode` setting. `yolo` auto-allows every capability;
`write` auto-allows read/write and confirms exec; `ask` confirms all three
(`crates/pi-agent/src/types.rs:140-185`). CLI overrides settings, whose default
is yolo (`crates/pi-cli/src/session_run_blueprint.rs:79-82,628-653`).

Production tools and extension registrations carry explicit `ToolCapability`
metadata; missing metadata defaults to exec rather than inferring from a name.
The host policy runs before the pre-existing hook and extension reducer. TUI
uses the confirmation broker; noninteractive modes fail closed whenever a
confirmation is required (`crates/pi-cli/src/approval.rs:16-66,125-263`).
Project `--approve`/`--no-approve` remains a separate resource-trust decision,
not an authorization-mode alias.

### MEDIUM — no auto-mode classifier

Grok ships `[auto_mode]` (prompt_type/classifier_model/classify_timeout,
degrades to asking rather than silently rejecting — `analysis.md:825,1770-1780`).
rpi has no prompt classifier; behavior is fixed at launch.

### LOW — prompt queue is steering/follow-up only; no user-facing queue management

rpi has a dual `steering` and `follow_up` message queue inside the agent loop,
governed by `QueueMode::OneAtATime` or `QueueMode::All`
(`crates/pi-agent/src/queue.rs:1-55`, `crates/pi-agent/src/agent.rs:247-250`).
`Application` exposes `queued_messages`/`drain_queued_messages` for inspection
and the TUI's `/dequeue` action restores queued steering and follow-up messages
to the editor (`crates/pi-coding/src/application.rs:1552-1559`;
`crates/pi-cli/src/tui.rs:6381-6394`).

The gap versus Grok's prompt queue is the user-facing management surface:
there is no enqueue/removal/reorder/edit/clear/send-now UI, no interruptible
wait barrier, and no persistence of queued prompts across restarts
(`analysis.md:1271-1273,1300-1303`). The current queues are internal scheduler
inputs rather than a standalone user prompt queue.

### LOW — no doom-loop recovery

Grok ships `[doom_loop_recovery]` (tool-loop recovery shared with remote
settings — `analysis.md:822`). rpi has `loop_scheduler` for scheduled loops
but no detection/recovery for a model stuck repeating tool calls.

### LOW — no AI suggestions

Grok ships `[suggestions]` (AI-generated shell suggestions via
AISuggest/SuggestPrompt side channels — `analysis.md:862,3677`). rpi has no
suggestion surface.

### By design — rpi workflows are worktree/YAML; grok/OMP use script engines

Grok's workflow engine is native Rhai scripting (`.grok/workflows/*.rhai`,
host API: `agent`/`parallel`/`phase`/`complete`/`pause`/`await_user`/`budget`/
`write_scratch_file`/`json_encode`, budgets 1-1024, `validate_only`,
`resume_from_run_id` — `analysis.md:4332-4361`). rpi workflows are
declarative YAML executed in git worktrees (`crates/pi-coding/src/workflow_worktree/`).
Both are valid designs; the gap is expressiveness (conditional/loop/user-gate
workflows are impossible in rpi's YAML DAG).

### MEDIUM — no role/persona system

Grok ships roles (`.grok/roles/*.toml`: model/effort/capability ceiling/
prompt file) and personas (behavior override + input/output contracts), with
AgentDefinition frontmatter (effort/promptMode/disallowedTools/maxTurns/
maxToolCalls/timeoutSecs/finalizeGraceSecs — `analysis.md:4298-4324`). rpi
has `AgentDefinition` with tools/skills/model but no capability ceilings,
disallowed tools, or turn/timeout contracts.

### LOW — no goal role-model pins

Grok's `[goal]` pins role models (planner/strategist/skeptic) and tracks
streaks/classifier state (`analysis.md:820,3742-3746`). rpi's `/goal`
(`crates/pi-cli/src/goal_commands.rs`) has a token budget only.

## Missing collaboration/extension infrastructure

### MEDIUM — no MCP support

Grok ships full MCP: `[mcp_servers.<name>]` config (transport/command/url/env),
disabled lists, managed gateway, Claude/Cursor MCP JSON import, `grok mcp`
command, dynamic registration + search_tool progressive discovery
(`analysis.md:849-851,939,1098,1048`). OMP ships an MCP runtime with
connection lifecycle, 250ms fast-start gate, DeferredMCPTool, and reconnects
(`omp://mcp-runtime-lifecycle.md`, `mcp-config.md`, `mcp-protocol-transports.md`).
rpi has no MCP at all (grep only hits OAuth scope strings). Extensions are
child-process JSONL instead.

**Impact:** rpi cannot reuse the ecosystem of MCP servers (filesystem,
github, slack, browser) that OMP/grok/Claude Code all speak.

### MEDIUM — no hooks system

Grok ships hooks (`$GROK_HOME/hooks/`, hooks-paths registry, project `.grok`
hooks, Claude/Cursor settings.json hooks, folder-trust gating —
`analysis.md:994-999,1099`). OMP ships a hook system too (`omp://hooks.md`).
rpi has no hook events beyond extension event hooks (child-process).

### LOW — no plugin marketplace

Grok ships plugins + marketplace (`[plugins]`, `[[marketplace.sources]]`,
`grok plugin` — `analysis.md:964-966,863,1049`); OMP ships a marketplace and
plugin manager (`omp://marketplace.md`, `plugin-manager-installer-plumbing.md`).
rpi has extension loading but no packaged-plugin market or install command.

### LOW — no collab/shared sessions

OMP ships `/collab`: AES-256-GCM encrypted live session sharing with a web
client and self-hosted relay (`omp://collab.md`). rpi has no session sharing;
`/share` exports a static HTML file.

### LOW — no ACP protocol

Grok converges all clients on the Agent Client Protocol (ACP: initialize/
authenticate/session/prompt + reverse requests; `grok agent stdio` for editor
embedding, `grok agent serve` WebSocket, leader mode — `analysis.md:1960-1964,
543-567,992,1211-1218`). rpi has its own JSONL RPC (`rpi-rpc`) which is
not ACP-compatible; editors that speak ACP cannot embed rpi.

## Missing config/CLI surface

### MEDIUM — no config profiles, managed config, or campaigns

OMP ships `--profile <name>` relocating the user base dir (`omp://config-usage.md:76-78`).
Grok ships managed config (`/etc/grok/managed_config.toml` +
`requirements.toml`, fail-closed, Ed25519-signed policy envelopes —
`analysis.md:676-680,1075,2787`), macOS MDM preferences, and remote campaign
overlays (`GROK_CAMPAIGNS_OVERRIDE`, `campaigns_state.json` FIFO —
`analysis.md:701-709`). rpi has a single JSON settings file.

### LOW — no TOML config or env expansion

Grok config is TOML with `$VAR`/`${VAR}` expansion and `[[version_overrides]]`
(`analysis.md:667-668`). rpi uses JSON `settings.json` with no env expansion
or versioned overrides.

### LOW — no `grok inspect`/`doctor`/`setup`/`dashboard` subcommands

Grok ships `grok inspect` (full config check — `analysis.md:1014-1041`),
`doctor` (environment diagnostics — `:1053`), `setup` (fetch managed config —
`:1047`), `dashboard` (`:1056,798`), and `memory`/`mcp`/`plugin`/`leader`/
`workspace` management families (`:1048-1056`). rpi's CLI has `sessions`/
`models`/`config`/`workflow`/`login`/`logout` but no inspection or diagnostics
commands.

### LOW — no shell completion generation

Grok ships `grok completions` for shell completion scripts (`analysis.md:1011`).
rpi has in-TUI completion but no `rpi completions` generator.

### LOW — no headless structured output

Grok headless supports JSON Schema structured output with pre-run validation
(`analysis.md:514-517`). rpi headless has text/JSON/JSONL event modes but no
schema-validated structured output.

### LOW — auth.json uses advisory locking and atomic writes; no scoped provider store

rpi writes `auth.json` through advisory locking and atomic temp+rename.
`AuthFileLock::acquire` creates a per-file `auth.json.lock` owner record, waits
with timeout/retry, and reclaims stale locks by detecting a dead pid, a
mismatched host/boot identity, or a different process start time
(`crates/pi-coding/src/auth.rs:280-343,435-467`). The lock is released via RAII
(`auth.rs:343`). `write_credentials_atomic` writes to a temp file and renames
it over the target (`auth.rs:1050-1075`). Focused regressions verify exclusive
access, stale-lock recovery across process restarts, and no token leakage in
lock files (`auth.rs:1284-1374`).

The remaining gap versus Grok is the data model: rpi stores a flat map of
credentials rather than a scoped store (BTreeMap scope→GrokAuth with OIDC/device/
WebLogin classifications) and does not isolate corrupt reads per scope
(`analysis.md:897-906,828,907-908`).

## Missing session/conversation features

### LOW — no session TTL cleanup

Grok auto-GCs sessions after `[storage] cleanup_ttl_days` (default 30 —
`analysis.md:978,859`). rpi retains sessions indefinitely.

### LOW — no rewind/checkpoint

Grok supports `rewind` session rollback (`analysis.md:1265`); OMP ships
`checkpoint`/`rewind` for collapsing exploratory context (`omp://tools/checkpoint.md`).
rpi has no rewind; the closest is forking a session.

### LOW — no snapcompact/elision compaction strategies

OMP ships snapcompact (archive dense history without LLM summary —
`omp://compaction.md:133-135`), useless-result elision
(`:165-167`), split-turn compaction (`:194-196`), and file-operation
context in summaries (`:254-256`). rpi compacts via LLM summary only.

### LOW — no `/fresh`, `/dump`, encrypted share

OMP ships `/fresh` (reset provider session without losing local transcript),
`/dump` (plain-text copy to clipboard), and E2E-encrypted `/share`
(AES-256-GCM + gist/blob + viewer page —
`omp://session-operations-export-share-fork-resume.md`). rpi's `/share` is
unencrypted static HTML.

### LOW — no handoff generation

OMP generates cross-session handoff summaries (`generateHandoff()` —
`omp://compaction.md:248-250`). rpi has no handoff concept.

### LOW — no secrets obfuscation

OMP obfuscates secrets with deterministic reversible placeholders
(`omp://secrets.md`). rpi passes raw secrets in transcripts.

## Priority roadmap summary

Ranked gaps against OMP/grok by impact and implementation effort. Details,
peer evidence, and risk levels are in the sections above.

| Gap | Impact | Effort |
|---|---|---|
| **MCP** | Cannot reuse the MCP ecosystem (filesystem/github/slack servers) | Medium |
| **LSP** | Model navigates by grep; no precise symbol jumps/renames | Medium |
| **Memory system** | No cross-session memory (OMP `learn`/`recall`/`reflect`/`retain`) | Medium |
| **browser/computer** | Cannot inspect web pages or drive the desktop | High |
| **Collab** | Cannot share a live session in real time | Medium |
| **Hooks** | No general event interception beyond extension hooks | Medium |
| **ACP** | Editors cannot embed rpi (grok speaks ACP; rpi speaks JSONL only) | Medium |

Approval modes are now the safety baseline. Suggested next order: MCP → hooks →
LSP → memory → collab/ACP → browser/computer (highest effort, last).

## Internal code quality: file granularity

Focused panels and control-plane code now live in separate modules, but the
central runtime files remain far above the 800-line investigation threshold.
Current counts from the working tree are:

| File | Lines | Over 800-line threshold |
|---|---:|---:|
| `crates/pi-cli/src/tui.rs` | 19,351 | 24.2× |
| `crates/pi-coding/src/orchestration/runtime.rs` | 5,028 | 6.3× |
| `crates/pi-coding/src/session.rs` | 4,524 | 5.7× |
| `crates/pi-coding/src/extensions.rs` | 4,043 | 5.1× |
| `crates/pi-coding/src/tools.rs` | 4,000 | 5.0× |
| `crates/pi-coding/src/session_store.rs` | 3,481 | 4.4× |
| `crates/pi-cli/src/modes/rpc.rs` | 3,409 | 4.3× |
| `crates/pi-coding/src/application.rs` | 3,431 | 4.3× |

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

The current inventory contains 102 classified entries: 22 `RESOLVED`, 10 `By
design`, 4 `PARTIAL`, 11 `HIGH`, 26 `MEDIUM`, and 29 `LOW`. Observable fixes
therefore account for 21.6% of all entries. Excluding deliberate divergences,
22 of 92 actionable/partially-actionable entries are resolved (23.9%). These
figures describe this document's headings, not test coverage or release
readiness.

## Verification

Compatibility claims cite the current working-tree implementation and the
pinned peer/upstream snapshots named in their sections. `RESOLVED` means an
observable regression exists and passed during this implementation wave. The
final focused gate observed: `pi-cli` library 562/562, code-review 52/52, core
TUI 8/8, side-chat 21/21, cross-tool TUI 4/4, terminal lifecycle 11/11,
session/RPC compatibility 11/11, approval 12/12, listen control plane 24/24,
JSON binary 1/1, RPC binary 16/16, plus `cargo check --locked -p pi-cli --lib
--tests`. Entries still marked `HIGH`/`MEDIUM`/`LOW` or `PARTIAL` retain their
documented gap; passing adjacent suites does not reclassify them.
