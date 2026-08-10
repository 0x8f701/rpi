# Changelog

All notable changes to `rpi` are documented in this file.

## [0.2.8] - 2026-08-10

### Added

- Built-in HTTPS for `--listen`: auto-generated self-signed TLS certificate
  (rcgen + ring) by default; `--listen-cert`/`--listen-key` for real certificates;
  `--listen-plaintext` to opt out of TLS. WebSocket (wss://) and collab work
  over TLS without an external reverse proxy.
- PTY interactive bash mode: `pty: true` + `input` parameter for commands that
  prompt (e.g. `sudo`); portable-pty backend with process group reaping,
  timeout/abort, and fallback to normal execution on spawn failure.
- Codex Live realtime voice via CLIProxyAPI: WebRTC SDP exchange
  (`RealtimeCreateCall`/`RealtimeCreateSession`/`RealtimeStop`) with sideband
  WebSocket for transcript streaming; mode-aware mic button in Web UI.
- Vision model delegation: non-vision models (e.g. DeepSeek) automatically
  delegate image inputs to a configured `visionModel` for text description.
- Web frontend: WebSocket heartbeat (30s ping, 60s dead detection, backoff
  reconnect), highlight.js code highlighting (17 languages), mobile CSS
  (sidebar drawer, touch targets, single-line composer), bash/tool cards,
  file upload, hold-to-talk voice input, session titles, host switcher with
  recent hosts, token configuration in Settings panel, rpi logo favicon.
- Wider scrollbars (14px desktop, 24px touch) with hover/active states.

### Changed

- `--listen` defaults to HTTPS (self-signed); non-loopback TLS binds no longer
  require `--listen-allow-insecure-remote`.
- Tokenless browser same-origin auth accepts both `http://` and `https://`
  origins.
- Orchestration features (tasks, process, todo, glob) enabled in global
  settings for Web mode.

### Fixed

- Self-signed private key cached with 0600 permissions; cache directory 0700.
- PTY stdin EOF: VEOF (0x04) written when no input provided so `cat` exits.
- PTY write errors now kill the child group instead of being silently ignored.
- PTY spawn failure with `input` configured fails closed (no silent fallback
  to non-interactive mode).
- Empty certificate chain rejected before TLS server config construction.
- Deprecated `subagents.agentOverrides` warnings silenced (migration still runs).
- Insecure remote control plane warning removed.


### Changed

- `rpi --listen` is now a Web-only, signal-owned service: it never acquires
  raw terminal state or starts the TUI/REPL, remains alive with closed stdin,
  rejects positional prompts, and shuts down cleanly on Ctrl-C or SIGTERM.
- The Web composer uses one primary Send/Steer action, keeps Abort active-only,
  and gives the textarea usable space on phone viewports.

### Fixed

- Web session switching consumes authoritative backend history for loaded and
  disk-resumed sessions; recorded Web conversations survive listener restarts.
- Web transcripts hide `display: false` internal custom messages, render typed
  orchestration notices without raw scaffolding, and bound bash/tool output to
  compact failure-tail views consistent with the TUI.

## [0.2.6] - 2026-08-09

### Fixed

- Dense Todo HUDs retain the compact `more active todos` overflow row when
  the live panel reaches its eight-row height cap instead of replacing that
  row with an unnecessary second blank separator.

## [0.2.5] - 2026-08-09

### Changed

- The embedded Web client now auto-connects without a token file. Local use
  needs only `rpi --listen 127.0.0.1:8765`; explicit LAN use needs only
  `rpi --listen 0.0.0.0:8765 --listen-allow-insecure-remote`. A configured
  `--listen-token-file` still makes authentication mandatory, and ACP browser
  restrictions remain unchanged.
- Tokenless browser requests use ordinary same-origin validation (`Origin`
  authority equals HTTP `Host`) while native clients without `Origin` remain
  supported. `--listen-advertised-origin` is now limited to collaboration
  links and the reachable URL printed for wildcard binds.

## [0.2.4] - 2026-08-09

### Added

- `/rewind [<entry-index|checkpoint-name>]` rolls the session back to before
  an entry: the record file is truncated at the cut (the dropped tail is
  archived verbatim to a `.rewind-<timestamp>.jsonl` sidecar first), and the
  in-memory transcript, todo list, goal state, session name, and the
  Responses stateful chain are rebuilt from the retained journal — a goal
  journal cut through at the rewind point re-derives the goal away. Bare
  `/rewind` lists the last 20 records with indices and first-line previews to
  pick from. Rewinding is refused past the first entry, while a prompt is
  processing, while bash is running, or while orchestration jobs or
  workflows are still active.
- `/checkpoint <name>` marks the current position as a named rewind target
  (a `checkpoint` journal record that never joins the linear record chain or
  the transcript); `/rewind <name>` rolls back to the marked position.
- `/fresh` starts a new clean session (alias for `/new`): the current session
  stays archived on disk and the TUI resets to the new recorder.
- `/dump [--jsonl] [path]` exports the current session through the existing
  export path — HTML by default, JSONL with `--jsonl` (or a `.jsonl` path),
  defaulting to the session-file-derived path for HTML and
  `cwd/<name>.jsonl` for JSONL.
- `/share --encrypt [passphrase]` writes a passphrase-encrypted
  AES-256-GCM `.jsonl.enc` share of the current session (nonce prefix +
  ciphertext; key = SHA-256(passphrase)), with an optional best-effort
  secret-gist upload when `gh` is available. The passphrase is never stored
  or logged.
- `/queue` shows the pending steering/follow-up prompts (counts plus
  per-item previews); `/queue cancel` clears them instead of restoring them
  into the editor. Works in the TUI and the line REPL while a turn streams.
- The idle composer status line now shows a subtle deterministic `Next:`
  suggestion (dim, one line, no model calls) derived from state: pending
  follow-up prompts → `/queue`, an active goal → `/goal`, live workflows →
  `/workflow list`.
- Doom-loop recovery: when the same tool fails with the same error three
  times in a row within a turn, the turn stops with an actionable message
  (`repeated failure — stopping; try a different approach or /undo`) instead
  of letting the model retry the failing call forever. Transient
  network/timeout errors never count toward the threshold.
- Opt-in Linux filesystem sandbox for the `bash` tool (`settings.sandbox` or
  the per-call `sandboxed` parameter; default off). Commands run inside
  `unshare` mount/pid/net namespaces confined to `sandbox.allowedPaths`
  (default: the working directory plus the agent directory), with
  `sandbox.deniedPaths` hidden and network off unless `sandbox.network` is
  true. Same-user confinement, not isolation; requires Linux and `unshare`.
- Model Context Protocol (MCP) client: session-scoped stdio servers declared
  under `settings.mcpServers` (Grok-compatible `[mcp_servers.<name>]` shape),
  the `mcp` tool for server discovery and tool calls, and `rpi mcp list` /
  `rpi mcp import` (Claude Desktop or Cursor configs) CLI commands. Configured
  `env` values are never echoed into tool output.
- Agent Client Protocol (ACP) v1 mode: `rpi agent stdio` (Content-Length
  framed JSON-RPC on stdin/stdout) and `rpi agent serve` (WebSocket) let
  ACP-speaking editors embed rpi, with `session/request_permission` approval
  round trips; rpi authenticates with configured credentials (`rpi-auth`) and
  never collects secrets over the wire. `agent serve` is loopback-only
  (plaintext WebSocket; TLS is tracked for a later release).
- Web chat client: `rpi --listen` serves a self-contained React/TypeScript
  app at `/web`, inlined into the binary and driving the existing WebSocket
  control plane through the `/rpc` and `/ws` routes. Authentication is
  optional: the tokenless loopback default (`rpi --listen 127.0.0.1:8765`)
  accepts browsers directly, so the page auto-connects with no token. An
  explicit tokenless LAN opt-in (`--listen 0.0.0.0:8765
  --listen-allow-insecure-remote`) allows tokenless LAN browser/RPC from any
  host/IP that routes to the listener; browsers are accepted when the request
  `Origin` authority equals the HTTP `Host` (ordinary same-origin, not
  authentication and not DNS-rebinding protection), with no
  `--listen-advertised-origin` required for `/web`, `/ws`, or `/rpc`. Adding
  `--listen-token-file <path>` makes the
  token mandatory on either bind (browser presents `rpi-auth.<token>`); rpi
  prints a startup warning that plaintext HTTP/WebSocket exposes any bearer
  token and control traffic to passive network observers. `rpi agent serve`
  remains loopback-only and still rejects tokenless browsers. Remote
  deployments should place the listener behind a TLS reverse proxy; this
  release does not provide TLS on `--listen`. Multi-session authoritative
  restore and dedicated panels for todo, goal, workflow, session tree,
  settings, subagent jobs, side chat, and maintenance are implemented.
- Encrypted live collaboration under `rpi --listen`: AES-256-GCM framed
  WebSocket relay with HKDF epoch keys, capability-hash subprotocol auth,
  control/view role links, `collab_start/status/stop` RPC, interactive `/collab`
  `/join` `/leave` commands in the TUI/REPL, and CLI/browser guest clients.
  Server-side snapshots and live events are privacy-redacted before guests see
  them, and the full E2E scenario passes with all control/view/browser guest
  assertions passing. Wildcard `--listen` binds (0.0.0.0 or `::`) require
  `--listen-advertised-origin <URL>` (a strict http/https origin) before
  `/collab` can print links; loopback binds advertise their local address
  automatically.
- Persona lifecycle: durable `<scope>/personas/<name>/persona.md` definitions,
  `/persona` command to create, switch, and manage personas, and persistent
  per-persona memory and session continuity.
- In-process QuickJS extension host (`runtime: "quickjs"` manifests) that
  replaces the Bun subprocess host: async runtime/context, ESM loader,
  per-runtime memory limits, cancellation, and the existing
  tools/commands/event-hooks/renderers/provider capabilities.
- Plugin marketplace: `rpi plugin install|list|remove|update` with directory,
  tarball, GitHub `owner/repo`, git URL, and `npm:<name>[@<version>]` sources.
  npm tarballs are verified against the registry's `dist.integrity` sha512
  digest; installs fail closed when it is missing or mismatched.
- Host hooks (`settings.hooks`) for `pre_tool_call`, `post_tool_call`,
  `session_start`, `session_end`, `turn_start`, `turn_end`, and
  `pre_trust_decision`, plus an extension trust hook that can recommend a
  trust decision.
- Memory backends: the local `memory` store and Hindsight-backed
  `recall`/`retain`/`reflect` tools, injected into model context.
- Extended tool catalog: LSP (hover/definition/references/diagnostics/
  symbols/rename/code actions), headless browser, GitHub, DAP debugger, eval,
  notebook, document conversion (PDF/DOCX/IPYNB), image generation and
  inspection, ast-grep structural search/rewrite, web search, and the `ask`
  tool.
- `rpi doctor` (text and `--json`) installation diagnostics and
  `rpi config get|set|reset|list` (OMP parity) for inspecting and changing
  settings from the CLI.
- Configuration profiles (`--profile`), TOML settings files, environment
  variable expansion in settings, and scoped per-provider auth keys.
- Overlayfs isolation backend for workflow checkouts on Linux (`worktree`,
  `overlayfs`, or `none`).
- Startup session TTL pruning (`sessionTtlDays`, default 30 days) for
  untouched native session files.
- Orchestration soft budgets (`maxRequests`, `maxTokens`, `yieldAfter`) that
  settle child jobs with a partial result instead of failing, and workflow
  Todo-DAG ownership (`WorkflowTaskOwnership {workflowId, todoTaskId}`) so
  concurrent workflows execute owned tasks without cross-workflow collisions.
- The RPC control plane gained settings-draft, workflow-lifecycle, queue,
  handoff, snap-compact, rewind, and steering commands.

### Changed

- The standalone `rpi-rpc` companion binary was removed; the JSONL RPC control
  plane is served by the `rpi rpc` subcommand (≡ `--mode rpc`).
- JS extensions run in-process via QuickJS instead of a Bun subprocess:
  Bun-hosted `.ts` fixtures were migrated to `.mjs` manifests with
  `runtime: "quickjs"`, and the Node/Bun runtime dependency is gone.
- Todo DAG execution reworked around a coordinator that runs ready tasks,
  joins on blocked dependencies, and keeps failed/cancelled owners open with
  idempotent terminal reconciliation; sessions switching away wait for owned
  orchestration jobs to settle.
- Natural-language routing: an exact agent mention (`Have <agent> ...`) spawns
  that agent and wins over overlapping skills; skill-only text never spawns.
- TUI rework: Todo DAG list/detail views, workflow master/detail panel,
  subagents panel with job cards, two-pane settings and two-column model
  selectors, sender → recipient IRC rendering, and rendering fixes (CJK
  padding, border contrast, sparse cyan palette, scrollback handling, no
  phantom user-message indent).
- `/handoff` now produces an English prose briefing (objective, todos, active
  jobs, recent asks) in addition to the structured envelope.
- Documentation restructured into an mdBook reference manual under
  `docs/src/` (including the `architecture.md` reference), and the E2E
  handbook gained user-scenario and web-client lanes.

### Fixed

- Session recovery: `/rewind` truncates the record file with an archived
  sidecar and rebuilds goal/todo/transcript state, refusing while prompts,
  bash, orchestration jobs, or workflows are active; `/snapcompact` fsyncs
  the snapshot before referencing it; startup TTL pruning no longer breaks
  fresh recorders.
- Doom-loop recovery stops a turn after repeated identical tool failures with
  an actionable message instead of retrying forever; transient network and
  timeout errors never count toward the threshold.
- Security: passphrase-encrypted session shares (`/share --encrypt`,
  AES-256-GCM), scoped auth keys, secret redaction in hook/MCP/extension
  diagnostics, and fail-closed trust decisions when a `pre_trust_decision`
  hook errors or times out.
- Workflow and runtime errors are redacted from user-visible output; hub wait
  delivery and concurrent provider-registration races resolved; settled
  orchestration jobs are retained within bounds and interrupted jobs are
  truthfully marked cancelled on recovery.
- Mermaid rendering no longer panics on multibyte CJK block keywords;
  fenced-code frames stream and settle correctly.

### Known limitations

- `npm:` package sources are not implemented for the `rpi install` package
  manager (the plugin marketplace accepts `npm:` sources via
  `rpi plugin install`).

## [0.2.3] - 2026-08-05

### Added

- `/code-review [<from> <to>]` can compare any two commits, branches, or tags
  while preserving the existing bare HEAD-to-working-tree view.
- Added persistent `/btw` side conversations, interactive host-tool approval,
  the authenticated `--listen` control plane, supervised PTY attachment, Todo
  DAG navigation, and agent management in the TUI.
- Added the deterministic auto-mode classifier (`selector.autoMode`:
  `off` | `suggest` | `auto`). It detects code tasks, plain questions, and
  long-running goals from the prompt; `suggest` shows a status hint
  (`Detected: code task — /todo to plan`) after the prompt, and `auto`
  additionally creates and starts a todo DAG for code tasks when
  orchestration is enabled and no todo list exists.
- `/workflow create <objective>` now accepts a single objective and uses it as
  the workflow name when no explicit name is provided.

### Changed

- Workflow creation starts supervision asynchronously, projects live status to
  the TUI, and redacts nested worktree/runtime errors from user-visible output.
- Session switches now wait for owned processes to terminate before committing
  a same-directory session cutover.
- Code review captures large tracked change sets without silently dropping late
  paths, keeps Git execution isolated, and bounds oversized diff output.

### Fixed

- Resolved hub wait delivery and concurrent faux-provider registration races.


## [0.2.2] - 2026-08-02

### Changed

- The managed binary install root is now `~/.rpi` on Unix and
  `%USERPROFILE%\.rpi` on Windows. Runtime configuration remains under
  `~/.pi/agent`, project `.pi/`, and the existing `PI_*` environment variables.
- Renamed the standalone RPC companion binary from `pi-rpc` to `rpi-rpc` and
  completed the user-visible `rpi` branding in README, docs, E2E handbook, and
  examples.
- Google desktop OAuth now uses the existing PKCE verifier without embedding a
  reusable client secret in source code or release binaries.
- Local package sources can be selected by absolute path for `rpi update` and
  `rpi remove` without a failed git-shorthand probe aborting identity matching.
- TUI `/run` dispatch no longer blocks extension UI requests while a command is
  awaiting a select, input, confirm, or editor interaction.

## [0.2.0] - 2026-08-01

### Changed

- Product binary cutover from `pi` to `rpi`. The published CLI executable,
  managed install path, and self-update activation target are now `rpi`
  (`~/.pi-rs/bin/rpi` on Unix, `%USERPROFILE%\.pi-rs\bin\rpi.exe` on Windows).
- Release assets are named `rpi-<version>-<target-triple>.tar.gz` (`.zip` on
  Windows) and contain a root-level `rpi` / `rpi.exe` binary plus `LICENSE`.
- One-line installers (`install.sh`, `install.ps1`) and `rpi update --self`
  download, checksum, smoke-test (`rpi <version>`), and atomically activate
  those `rpi-*` assets under `PI_HOME` (default `~/.pi-rs`).
- Workspace package version is `0.2.0`. Manual install from source is
  `cargo install --path crates/pi-cli --locked --bin rpi`.
- The headless RPC companion binary remains `pi-rpc`. Runtime configuration
  paths and environment variables are unchanged: `~/.pi/agent`, project
  `.pi/`, and `PI_*`.

- Added durable multi-workflow orchestration with isolated git worktrees,
  workflow-owned Todo DAGs and subagents, typed RPC lifecycle commands,
  list-to-detail TUI navigation, IRC projections, and explicit conflict state.
- Reworked the inline TUI composer for immediate printable input, bounded large
  paste handling, grouped undo, Ctrl-U clear, prompt history, clipboard image
  attachments, compact live task cards, and non-durable error toasts.
- Goal create/resume now activates model work immediately. Trusted skills remain
  namespaced under `/skill:`, including `coordinate` discovery and execution.

### Migration

- Fresh installs create only the `rpi` command. On Unix, `install.sh` removes a
  legacy installer-managed `~/.pi-rs/bin/pi` symlink only when it still points
  at a previous installer-owned download path; unmanaged `pi` commands are left
  alone.
- Prefer `rpi` in scripts and docs. Existing agent config under `~/.pi/agent`
  continues to work without relocation.

### Known limitations

- `npm:` package sources are not implemented; attempting to install one fails
  with a clear error.

## [0.1.0] - 2026-08-01

### Added

- Self-contained `pi` CLI with print mode, line REPL, full-screen TUI, and
  headless JSON/RPC machine-readable modes.
- Native Pi v3 session storage: append-only JSONL files with `parentId` branch
  support, resume (`--resume`, `--continue`), listing (`pi sessions`), import
  (`pi import-session`), export (`pi export`), and private gist sharing
  (`/share`, share events).
- `pi import-session` for converting `pi`, `omp`, `codex`, `claude`, `grok`,
  and `droid` sessions to native JSONL.
- Default coding tools: `read`, `bash`, `edit`, and `write`; optional built-in
- Multi-provider streaming support: OpenAI chat completions, OpenAI Responses,
  Anthropic Messages, and Google Generative AI.
- Embedded model catalog with `pi models [filter]` listing plus local model
  integration through a configured llama.cpp router (`pi llama` subcommands).
- Custom provider/model configuration via `models.json` (OpenAI-compatible
  proxies, Cloudflare AI Gateway, custom Anthropic/Google setups).
- Authentication resolution across `--api-key`, runtime keys, `auth.json`,
  `models.json`, provider-specific environment variables, and interactive
  `pi login`/`pi logout` with template expansion and redaction.
- Project trust system with `trust.json`, `--approve`/`--no-approve`, and
  `defaultProjectTrust` in settings.
- Two-phase global/project `settings.json` loading with merged packages,
  extensions, skills, prompts, themes, compaction, and terminal options.
- Project context file discovery (`AGENTS.md`, `CLAUDE.md`) and `.pi/skills`
  skill loading for the system prompt.
- Automatic context compaction with configurable token limits.
- Faux provider for deterministic tests and examples.
- Local and git Pi package backend with `pi install`, `pi remove`, `pi list`,
  `pi update --extensions`, and positional `pi update PACKAGE`. npm sources are
  explicitly rejected until a dedicated backend is added.
- Process extension protocol with `pi-extension.json` manifests, newline-
  delimited JSON stdin/stdout framing, and capabilities for tools, commands,
  event hooks, message renderers, provider metadata, and UI widgets.
- Custom TUI themes (`dark`, `light`, and JSON theme files) and custom
  keybindings (`keybindings.json`) with validation.
- Native self-update via `pi update --self` with SHA-256 verification,
  atomic activation, and rollback.
- One-line installers for macOS, Linux, and Windows with SHA-256 verification,
  atomic symlink activation, and rollback on failure.
- Runnable examples covering JSON event consumption, JSON-RPC-style event
  wrapping, system-prompt templates, and custom tools.
- Initial documentation set: README, install guide, quickstart, CLI modes,
  settings/trust, authentication, models, RPC/JSON events, TUI, prompt
  templates, skills, update safety, export/share, local/self-hosted models,
  extensions, packages, security, and environment variables.

### Known limitations

- `npm:` package sources are not implemented; attempting to install one fails
  with a clear error.

[0.2.7]: https://github.com/0x8f701/rpi/releases/tag/v0.2.7
[0.2.8]: https://github.com/0x8f701/rpi/releases/tag/v0.2.8

[0.2.6]: https://github.com/0x8f701/rpi/releases/tag/v0.2.6
[0.2.5]: https://github.com/0x8f701/rpi/releases/tag/v0.2.5
[0.2.4]: https://github.com/0x8f701/rpi/releases/tag/v0.2.4
[0.2.3]: https://github.com/0x8f701/rpi/releases/tag/v0.2.3
[0.2.2]: https://github.com/0x8f701/rpi/releases/tag/v0.2.2
[0.2.1]: https://github.com/0x8f701/rpi/releases/tag/v0.2.1
[0.2.0]: https://github.com/0x8f701/rpi/releases/tag/v0.2.0
[0.1.0]: https://github.com/0x8f701/rpi/releases/tag/v0.1.0
