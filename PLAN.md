# PLAN.md — rpi Web UI, Personas & Execution-Environment Decoupling

Roadmap for `rpi` (pi-rs), grounded in the current codebase at HEAD `58c6e8f`
(working tree includes uncommitted changes that do not affect the areas referenced
here). This document plans only; it changes no code.

---

## 0. Priority 0 — rpi Web UI (最高优先级, 尽快实现)

A browser UI to interact with rpi end to end (chat/transcript, tool activity, model
switch, todo/goal/workflow views, approvals), so users are not tied to the terminal.

**Rationale**: the backend surface already exists — `--listen <addr>` serves the JSONL
control plane over HTTP `POST /rpc` + WebSocket `/ws` with the token-auth pre-auth
boundary (`crates/pi-cli/src/modes/listen.rs`), and the RPC command set (50+ commands:
prompt/steer/follow_up/abort/model/todo/goal/workflow/process/loop) plus the async event
stream (`crates/pi-cli/src/modes/rpc.rs`) covers the interaction vocabulary. HTML export
already contains a self-contained markdown renderer (`crates/pi-coding/src/export/mod.rs`)
reusable for message rendering.

**Deliverables**:
1. Serve a self-contained static HTML+JS client from the `--listen` server (GET /), or
   as a standalone `rpi web` command; the client connects to the existing WS control
   plane and drives chat (prompt/steer/abort), streams events (agent/tool/todo/goal),
   renders messages (markdown + tool cards) reusing the export renderer's HTML path.
2. Close the RPC gaps the web UI needs (settings get/set, approval responses if not
   already wired, session list/switch).
3. Security: WS auth (token), XSS-safe rendering of model output (never trust model
   text), same pre-auth boundary as `--listen` today.
4. Tests: WS round-trip through the real binary, static page served, sanitization
   of model output, token auth.

**Estimate**: static-client slice ~2-3 person-days (backend: serve page + WS passthrough;
frontend: chat UI + event stream + markdown rendering); full views (todo/goal/workflow
panels) +2-3 days.

---

## 1. Overview

The roadmap is:

1. **Phase 1 — Persistent subagents ("personas")**: a named, durable subagent definition
   that survives sessions — identity, system prompt, tool set, model/thinking, role
   contracts, soft budgets, and critically persistent memory + a transcript archive that
   accumulate across runs.
2. **Phase 2 — Execution-environment decoupling**: let users route the agent's exec tools
   (bash first, then filesystem tooling, then orchestration children) to a user-defined
   execution environment — e.g. machines on the LAN — via SSH or a small `rpi runner`
   daemon, with the existing local sandbox remaining the default.

The order is deliberate. Phase 1 sits directly on the orchestration layer that already
exists: `AgentDefinition` already carries identity/prompt/tools/model/contracts
(`crates/pi-coding/src/orchestration/definitions.rs:66-89`), the runtime already enforces
role contracts and soft budgets (`crates/pi-coding/src/orchestration/runtime.rs:145-191,
2947-3015`), and durable child sessions already prove crash-safe transcript persistence
(`crates/pi-coding/src/orchestration/persistence.rs:24-31`). What is missing is a
*persistent identity*: today every `task` spawn is a fresh one-shot child whose artifacts
are pruned by retention (`runtime.rs:1026-1072`). Personas add a stable namespace —
definition + memory + transcript archive — that outlives any single job.

Phase 2 then builds on the exec boundary that Phase 1's personas will also use. Today that
boundary is strictly local: the bash tool runs either in-process through the embedded
brush shell (`crates/pi-coding/src/tools/bash/brush.rs:22-43`) or as a `/bin/bash -c`
subprocess (`crates/pi-coding/src/tools.rs:1723`), optionally confined by the Linux
sandbox (`crates/pi-coding/src/sandbox.rs:33-113`). Decoupling replaces "the local host is
the only execution target" with a pluggable `execution.environments` table while keeping
the local path (and its sandbox) as the default, so nothing that exists today changes
behavior until a user opts into a remote environment.

---

## 2. Current state summary (what these phases build on)

### 2.1 Orchestration subagents (task/hub tools, durable runtime)

- **`task` tool** spawns one or more independent child coding-session jobs and returns
  immediately with stable job/agent ids; **`hub` tool** supervises them
  (`crates/pi-coding/src/orchestration/tools.rs:38-46, 71-73`).
- **`hub read_history`** renders a bounded transcript (default 50, max 200 lines, 32 KiB
  cap) from the child's durable JSONL or the settle-time `.history.json` snapshot
  (`runtime.rs:37-43, 349-360, 4733-4737`).
- **Durable runtime**: child sessions are recorded under
  `<session-root>/children/<parent-id>/` with an atomic versioned sidecar
  `orchestration-state.json`; recovery is fail-closed (crash-interrupted turns are parked
  and jobs cancelled, never claimed exactly-once) (`persistence.rs:24-31, 144-185`).
- **JobSnapshot** is the persisted, serialized job state including
  `soft_budget_exhausted` (`crates/pi-coding/src/orchestration/jobs.rs:63-91`).
- **Soft budgets**: `JobSoftBudget` (max_requests / max_tokens / yield_after) is opt-in
  and settles a child as `Completed` with a partial result and the soft-limit marker
  instead of failing it (`runtime.rs:145-191`); surfaced as
  `settings.orchestration.softBudget.*` (`crates/pi-coding/src/settings_catalog.rs:385-387`).
- **Role contracts** (`max_turns`, `max_tool_calls`, `timeout_secs`, `disallowed_tools`,
  `capability_ceiling`) are parsed from definition frontmatter
  (`definitions.rs:580-663`) and enforced at runtime: wall-clock timeout with abort grace
  (`runtime.rs:1788-1844`), per-turn/per-tool-call stop hooks (`runtime.rs:2947-3015`),
  capability-ceiling tool filtering at spawn (`runtime.rs:283-288`). Contract tests live in
  `crates/pi-coding/tests/role_contracts.rs`.
- Child artifacts (`.md`) and history snapshots (`.history.json`) are written to
  `<cwd>/.pi/artifacts/<agent>-<job>.md|.history.json` (`application.rs:2502-2506`,
  `runtime.rs:1607-1617, 1872-1897`) and pruned by retention (`runtime.rs:1026-1072`).
- Coverage: `tests/durable_child_sessions.rs`, `tests/durable_orchestration_e2e.rs`,
  `tests/hub_read_history_e2e.rs`, `tests/orchestration*.rs`, `tests/live_delegation_e2e.rs`.

### 2.2 AgentDefinition / persona-adjacent pieces

- **`AgentDefinition`** (`definitions.rs:66-89`): `name`, `description`, `system_prompt`,
  `tools`, `autoload_skills`, `model`, `thinking_level`, `max_turns`, `max_tool_calls`,
  `timeout_secs`, `disallowed_tools`, `capability_ceiling`, `source`, `path`, `trusted`.
- **Discovery** (`definitions.rs:137-161`): project `<cwd>/.pi/agents/` (only when
  `project_trusted`), user `<agent_dir>/agents/` (trusted), plus the bundled `task` agent
  (`definitions.rs:502-508`); name collisions resolve in that precedence order.
- **`/role` command**: maps 1:1 onto loaded agent definitions — List / Show / Select /
  Clear / Current, where `--select` prefers a role for the next unnamed `task` spawn
  (`crates/pi-cli/src/interactive_commands.rs:184-203, 240-298`; dispatch at
  `repl.rs:508-514` and `tui.rs:8751-8769`).
- **Agents panel** in the TUI lists definitions with source and per-agent settings editing
  (`crates/pi-cli/src/agents_panel.rs:26-61`, `tui.rs:5295-5311, 6965-7013`).
- **`settings.agents.<name>`** overrides enabled/model/tools per definition
  (`crates/pi-coding/src/settings.rs:420-430`).
- Agent dir = `<home>/.pi/agent` or `$PI_CODING_AGENT_DIR`, profile-relocatable
  (`crates/pi-coding/src/resources.rs:97-122`); `CONFIG_DIR_NAME = ".pi"`
  (`resources.rs:19`).

### 2.3 Execution environment (bash tool, sandbox, process extensions)

- **bash tool** has two local execution paths sharing one bounded-output contract:
  - *Embedded brush* (in-process shell, `crates/pi-coding/src/tools/bash/brush.rs:22-43`):
    one brush session per invocation, explicit environment (no host inheritance), no
    profile/rc loading, merged stdout+stderr streaming, timeout/abort descendant reaping,
    and host guards that refuse `exec`/`suspend`/`ulimit`/`umask` and guard `kill` against
    the host pid. Fallback to the subprocess path on parse failure / non-Linux.
  - *Subprocess*: `/bin/bash -c` (or `sh -c`) via `run_bash_core`
    (`crates/pi-coding/src/tools.rs:1723-1760, 1611-1614`).
  - Output is bounded in memory with a rolling tail and spills to a temp file past limits
    (`crates/pi-coding/src/tools/bash/mod.rs:15-102`).
  - Tool registration: `bash_tool` with `ToolCapability::Exec`
    (`tools.rs:1500-1530`); public `execute_bash` entry (`tools.rs:2571-2589`).
- **Sandbox** (`crates/pi-coding/src/sandbox.rs:33-113`): Linux-only confinement via
  `unshare` (fresh mount/pid/net namespaces, tmpfs root + `pivot_root`, deny-by-default
  filesystem, loopback-only network) — *confinement, not isolation*; fail-closed
  validation; `run_in_sandbox` (line 275) and `spawn_piped` for process extensions
  (line 470). The sandbox wins over brush when active (`brush.rs:25-29`).
- **`settings.orchestration.sandboxed`** confines orchestration children's process spawns
  (their bash tool) to workspace + agent dir + `sandbox.allowedPaths`
  (`settings.rs:301-302`, `settings_catalog.rs:388`, child resolver at
  `session.rs:1382-1423`).
- **Process extensions** (`crates/pi-coding/src/process/`): supervised long-lived child
  processes via `ProcessManager` / `process` tool (`process/mod.rs`, `process/manager.rs`,
  `process/tool.rs`), gated by `settings.orchestration.process`
  (`settings_catalog.rs:378`).
- **Settings plumbing**: `Settings` struct with `extra` passthrough for unknown keys
  (`settings.rs:873-968`), runtime snapshot (`settings.rs:488-515`), and the declarative
  settings catalog (`settings_catalog.rs:339-398`) that drives RPC/TUI settings views.

### 2.4 Memory, sessions, and CLI/TUI surfaces relevant to the plan

- **Memory**: JSONL store per namespace under `<agent_dir>/memory/<repo-digest>/entries.jsonl`
  (`crates/pi-coding/src/memory.rs:3-7, 79-83, 118-136`); namespace = hex SHA-256 of the
  git-anchored cwd so memory persists across sessions in the same checkout
  (`memory.rs:289-302`); bounded (100 entries, 1 MiB per entry, 32 KiB recall output).
- **Sessions**: native Pi v3 append-only JSONL (`session_store.rs:29`), native sessions
  under `<agent_dir>/sessions/` (`session_catalog/mod.rs:501-502`); session ids validated
  (`session_store.rs:1951-1954`).
- **CLI/TUI**: `rpi` top-level flags + first-class subcommands
  (`crates/pi-cli/src/args.rs:23-214, 297-460`); primary slash surface
  (`interactive_commands.rs:111-130`); TUI panels (`tui.rs`), REPL (`repl.rs`).

---

## 3. Phase 1 — Persistent Subagents ("Personas")

### 3.1 What a persona IS

A **persona** is a named, persistent subagent definition that survives sessions. It is the
`AgentDefinition` model (`definitions.rs:66-89`) extended with durable state, so a persona
carries:

- **Identity**: `name` (validated as today, `definitions.rs:580-663`), `description`.
- **System prompt + personality/instructions**: the prompt body after frontmatter (the
  existing `system_prompt` field, `definitions.rs:69`); an optional `personality`
  frontmatter field may be added and rendered as an explicit section of the assembled
  system prompt so personality and instructions stay separable and auditable.
- **Tool set**: `tools` allow-list, `disallowed_tools`, `capability_ceiling`
  (read/write/exec), `autoload_skills` — all existing fields (`definitions.rs:70-85`).
- **Model / thinking**: `model` patterns + `thinking_level` (`definitions.rs:72-74`),
  with `settings.agents.<name>` overrides honored (`settings.rs:420-430`,
  `definitions.rs:resolve_agent_model`).
- **Role contracts**: `max_turns`, `max_tool_calls`, `timeout_secs` — already enforced at
  runtime (`runtime.rs:1788-1844, 2947-3015`) and covered by
  `crates/pi-coding/tests/role_contracts.rs`.
- **Soft budgets**: a per-persona `JobSoftBudget` (max_requests / max_tokens /
  yield_after, `runtime.rs:155-158`) so a persona can be capped below the global
  `settings.orchestration.softBudget.*` (`settings_catalog.rs:385-387`).
- **Persistent memory/state across sessions**: its own memory namespace (Section 3.3)
  that accumulates `learn`/`recall` entries across runs, and its own transcript archive
  that `hub read_history` can read after the run is long gone.

### 3.2 How a persona differs from today's ephemeral task-spawned subagents

Today, `task` spawns a child from a catalog definition + one-shot assignment
(`tools.rs:38-46`); when the job settles, the child's `.md` artifact and `.history.json`
snapshot live under `<cwd>/.pi/artifacts/` (`runtime.rs:1607-1617`) and are deleted by
retention/cleanup (`runtime.rs:1026-1072`). The next spawn is a blank slate: no memory, no
transcript continuity, no identity beyond the definition file.

A persona spawn differs in three observable ways:

1. **Identity is the archive, not the job.** Spawning `task { agent: <persona> }` binds
   the child to `<agent_dir>/personas/<name>/` (or `<cwd>/.pi/personas/<name>/`); the
   job's `agent_id` remains the per-run child id (as today, `jobs.rs:63-91`) but the
   persona's definition + memory + transcript outlive the job.
2. **Memory carries across runs.** The child's `memory`/`recall`/`retain` tools
   (`TOOL_NAMES`, `tools.rs:153`) resolve to the persona namespace
   (`<agent_dir>/personas/<name>/memory/entries.jsonl`, reusing the `MemoryStore`
   machinery, `memory.rs:118-136`) instead of the repo-digest namespace, so run N+1 sees
   what run N learned. [Inference: exact tool wiring will follow the existing
   `MemoryStore` default_for pattern, `memory.rs:116-124`.]
3. **Transcript continuity.** Each run appends a numbered JSONL record under
   `<agent_dir>/personas/<name>/sessions/` (native Pi v3 format, `session_store.rs:29`).
   `hub read_history` (already format-sniffing between durable child JSONL and
   `.history.json`, `runtime.rs:4733-4737`) gains a third resolution rule for persona
   archives, so any prior run of the persona is readable. On spawn, the tail of the
   persona transcript (bounded, e.g. the same 200-line/32 KiB caps, `runtime.rs:37-43`)
   is prepended to the child's context, giving the persona conversational continuity.

### 3.3 Storage layout

```
<agent_dir>/personas/<name>/          # user scope (trusted, like <agent_dir>/agents)
  persona.md                          # frontmatter + system-prompt body (existing parser)
  memory/entries.jsonl                # persona memory namespace (MemoryStore, bounded)
  sessions/                           # durable transcript archive, one JSONL per run
    <run-id>.jsonl

<cwd>/.pi/personas/<name>/            # project scope (only when project_trusted, like
                                      # <cwd>/.pi/agents, definitions.rs:144-148)
```

- Persona discovery mirrors `AgentCatalog::discover` (`definitions.rs:137-161`): project
  scope wins over user scope on name collisions; both use `parse_agent_definition`
  (`definitions.rs:580-663`) so the frontmatter grammar is identical to agents (plus the
  new `personality` and `softBudget` keys, parsed with the same
  `parse_positive_contract` helpers, `definitions.rs:748-760`).
- The memory namespace reuses `validate_namespace` path-safety rules
  (`memory.rs:315-326`); persona names already pass through `validate_name`
  (`definitions.rs:580-663`), so no new traversal surface.
- Run ids reuse the existing UUID-v7 job/child id generation (`persistence.rs:23`,
  `runtime.rs:879`).

### 3.4 Lifecycle

| Action | Surface | Notes |
|---|---|---|
| Create | `/persona new <name>` (opens editor) or hand-written `persona.md` | validated by `parse_agent_definition`; duplicate name in scope → actionable error |
| Edit | `/persona edit <name>` | live reload on next catalog discovery (same semantics as `/role`, `tui.rs:8751-8769`) |
| List / details | `/persona`, `/persona <name>` | rendered like `format_role_list` / `format_role_details` (`interactive_commands.rs:296-320`) |
| Select | `/persona <name> --select` | sets the same role preference as `/role --select` (`interactive_commands.rs:196-203`), so unnamed `task` spawns default to it |
| Run | `task { agent: <name>, task: ... }` or `/persona <name> run <assignment>` | spawns through the existing runtime; child inherits persona definition + memory tail + transcript tail |
| Supervise | `hub jobs/wait/cancel/read_history` | unchanged surface; `read_history` resolves the persona archive |
| Retire | `/persona remove <name>` | requires confirmation; memory/transcript archive kept by default, `--purge` deletes it |

Concurrency and limits: persona spawns go through the existing semaphore
(`settings.orchestration.maxConcurrency`, `settings_catalog.rs:381`) and recursion-depth
guard (`settings_catalog.rs:382`); persona contracts (`max_turns`, `timeout_secs`, soft
budget) are enforced by the existing runtime machinery, so a persona can never exceed its
ceiling by being spawned many times in one session.

### 3.5 TUI/CLI surface

- New `/persona` slash command family added to `PRIMARY_COMMAND_NAMES`
  (`interactive_commands.rs:111-130`) and dispatched in `repl.rs`/`tui.rs` alongside
  `/role` (`repl.rs:508-514`, `tui.rs:8751-8769`).
- The agents panel (`agents_panel.rs:26-61`) gains a persona section (source marker +
  per-persona contract summary + memory/transcript entry counts); selection and settings
  editing reuse the existing panel key handling (`tui.rs:6965-7013`).
- `hub read_history` output identifies persona runs with the persona name so the parent
  can distinguish runs (`read_history` already renders bounded, redacted transcripts,
  `runtime.rs:4736-4760` area, `tools.rs:349-360`).

### 3.6 Design decisions and open questions

- **Identity across sessions vs forks**: a persona run is a fresh child session bound to
  the persona archive — the persona itself is never a "live" agent between runs. This
  keeps the durable-runtime recovery model unchanged (`persistence.rs:24-31`): an
  interrupted persona run is parked/cancelled exactly like any child.
- **Memory bounds**: reuse the existing per-namespace bounds (100 entries, 1 MiB/entry,
  oldest evicted on learn, `memory.rs:79-83`); add an optional `memoryLimit` frontmatter
  key (entries) per persona, defaulting to the global bound. Open question: whether
  persona memory should also be scoped per repo (combined namespace, e.g.
  `<persona>:<repo-digest>`) — default is unscoped (persona-level), with repo scoping as
  a follow-up only if users report cross-project bleed.
- **Trust/approval**: user-scope personas are trusted like user agents
  (`definitions.rs:151-152`); project personas load only under `project_trusted`
  (`definitions.rs:144-148`); persona *actions* (file writes, exec) go through the
  existing approval/trust machinery unchanged — personas add no trust bypass.
- **Soft budget semantics**: per-persona budget replaces the global budget when present
  (global remains the fallback); marker propagation stays as today
  (`jobs.rs:63-91`, `runtime.rs:145-191`).
- **Open questions**: (a) should `/persona new` bootstrap from an existing definition
  (`--from <role>`)? (b) should persona transcripts participate in session TTL pruning
  (`session_ttl_days`, `settings.rs:895-898`) or be exempt? (c) rename/move of a persona
  with existing memory — copy vs move semantics (default: `mv` of the directory with a
  name-scoped lock, matching the settings-store atomic-write conventions,
  `settings.rs:2982-2988` area).

---

## 4. Phase 2 — Execution-Environment Decoupling (custom execution environments)

### 4.1 What decoupling means

Today the agent's exec tools run exclusively on the local host: bash via in-process brush
(`brush.rs:22-43`) or `/bin/bash -c` (`tools.rs:1723`), optionally inside the Linux
sandbox (`sandbox.rs:33-113`). Decoupling introduces an **execution environment** — a
named, user-defined target for the agent's process-spawning tools (bash first; then
filesystem tooling `read`/`write`/`edit`/`grep`; then process extensions; then
orchestration children). The local environment stays the default and is byte-for-byte
what exists today.

Target shapes:

1. **Local** — current behavior (brush + subprocess + sandbox). Default.
2. **LAN machines via SSH** — `ssh user@host` running the agent's commands remotely; the
   simplest first remote shape, no new binary on the remote side (needs `bash`/`sh` and
   standard tools only).
3. **LAN machines via a runner daemon** — a small `rpi runner` binary on the remote host
   speaking a JSON-RPC protocol (stdio or TCP/WebSocket); enables capability negotiation,
   cancellation, path-containment, and later tool routing beyond bash.
4. **Containers/VMs** — future shape, same runner protocol; out of scope for the first
   implementation but the protocol must not preclude it.

### 4.2 Protocol design (`rpi runner`)

A new `runner` subcommand on the CLI surface (`args.rs:297-460`), shaped like the existing
`agent` subcommand (ACP stdio = JSON-RPC over stdin/stdout with Content-Length framing;
`serve` = JSON-RPC over WebSocket, `args.rs:507-520`):

- **Transport**: `rpi runner --stdio` (spawned by the agent, inherited-fd auth) or
  `rpi runner --listen tcp://<addr> --secret-file <path>` (daemon mode, shared-secret
  auth). WebSocket framing for the daemon mode mirrors the ACP `serve` transport.
- **Methods**:
  - `ping` — handshake + version + fingerprint.
  - `capabilities` — OS, arch, shell path, available tools (bash, git, python3, ...),
    writable roots, path policy — the environment's *identity* the agent can display and
    negotiate against.
  - `exec { command, cwd, env, timeoutMs }` → `{ exitCode, output, truncated, cancelled }`
    — same bounded-output contract as the local path: in-memory rolling tail + spill file
    (≤ `MAX_FULL_OUTPUT_DISK_BYTES` = 10 MiB, `tools/bash/mod.rs:99-102`), output
    truncated like `truncate_tail` (`truncate.rs`).
  - `cancel { execId }` — remote process-tree cancellation (the runner reaps descendants
    the way the local paths do, `brush.rs:130-150` area).
  - `read` / `write` / `ls` / `rm` — filesystem bridging for remote cwd tooling, resolved
    against the runner's allowed roots (Section 4.3).
  - `shutdown`.
- **Command flow**: agent → runner: framed request; runner: spawns on the remote host,
  streams bounded output back in framed chunks; agent: feeds the same
  `OutputAccumulator` used locally (`tools/bash/mod.rs:15-102`) so truncation, spill, and
  redaction behave identically.
- **Environment identity + capability negotiation**: the handshake fingerprint
  (`hostname`, OS/arch, tool list, path policy) is presented to the agent in the bash
  tool's environment banner (like the session `PI_*` env, `brush.rs:22-43`), so the agent
  can adapt (e.g. "no `rg` on this host").

### 4.3 Security model

- **Auth between rpi and runner**: daemon mode requires `--secret-file` (0600, like
  auth.json conventions, AGENTS.md security #6); stdio mode authenticates by inherited
  fds (no secret on the wire). SSH mode uses the user's configured `keyPath` or the SSH
  agent. Secrets never travel on command lines (the sandbox already codifies "only paths
  are passed on the command line — never secrets", `sandbox.rs:119-123`).
- **Transport**: `runnerUrl` must be `ws://`/`wss://` on loopback or `wss://`/`https://`
  otherwise; plaintext remote endpoints are rejected with an actionable error (mirrors
  the live `allowInsecure` precedent, `settings.rs:386-389`).
- **Path containment on the remote side**: the runner resolves every path argument against
  a configured allowed-root set (fail-closed, `resolve_scoped_path` /
  `canonicalize_child_path` conventions per AGENTS.md security #4); the cwd must be inside
  an allowed root (mirrors `SandboxConfig::validate`, `sandbox.rs:69-113`).
- **No secrets in transit/at rest**: env values are sent only for the exec call (never
  logged); tool outputs and transcripts are redacted exactly as local
  (`redact.rs`, `tool_presentation.rs`); the runner writes no persistent state by default.
- **Remote capability limits**: per-environment timeout cap, output cap, and an explicit
  `allowProcessExtensions` flag (process extensions off by default remotely); the runner
  is a separate auditable binary with no host credentials baked in.

### 4.4 Settings

```toml
[execution]
default = "local"  # or a named environment

[[execution.environments]]
name = "lab-box"
kind = "ssh"        # local | ssh | runner
host = "lab-box.example.org"
user = "alice"
keyPath = "<path-to-ssh-key>"
allowed = true

[[execution.environments]]
name = "build-runner"
kind = "runner"
runnerUrl = "wss://build-runner.example.org:8443/rpi"
secretFile = "<path-to-runner-secret>"   # 0600
allowedRoots = ["/srv/builds", "/var/tmp"]
allowed = true
```

- Implemented as `Settings.execution` (an `ExecutionSettings` struct with `default` +
  `environments: Vec<ExecutionEnvironmentConfig>`), following the `Settings` struct
  pattern with `extra` passthrough (`settings.rs:873-968`) and catalog registration
  (`settings_catalog.rs:339-398` pattern, including a `Secret`-marked `secretFile`).
  Unknown `kind` values fail validation at load with an actionable message.
- **Per-session/per-tool selection**: `/environment <name>` switches the session default
  (persisted via the runtime-settings snapshot path, `settings.rs:488-515`); the bash
  tool accepts a per-call `environment` parameter (like the existing `sandboxed`
  parameter, `SandboxSettings`, `settings.rs:252-256`); orchestration children inherit
  the parent session's environment (new field in `ChildSessionOptionsSnapshot`,
  `session.rs:1368-1381`).
- Enabling an environment requires explicit `allowed = true`; entries default to
  disabled, and the local environment can never be disabled.

### 4.5 Impact on sandbox / orchestration / brush

- **Brush is in-process** (`brush.rs:22-43`) and cannot run on a remote host: remote
  environments force the subprocess/runner path. This is a documented, deliberate
  asymmetry: the local default keeps brush; any non-local environment routes through the
  runner (or SSH), which is also where the sandbox's confinement semantics are replaced
  by the runner's allowed-roots containment.
- **Sandbox vs remote are mutually exclusive per call**: an active local sandbox wins for
  the local environment (`brush.rs:25-29`); a remote environment never composes with the
  local `unshare` sandbox — the runner's path policy is its sandbox. `sandboxed` +
  non-local `environment` in one call is rejected at validation.
- **`orchestration.sandboxed`** (`settings.rs:301-302`, `session.rs:1382-1423`) continues
  to apply to local children only; remote children are governed by the environment's
  allowed roots and capability limits.
- **Process extensions** (`process/mod.rs`) gain a remote backend only in phase 4.3
  (runner with `allowProcessExtensions`); SSH mode never hosts process extensions.

### 4.6 Phased implementation

1. **Exec-boundary refactor (local-only)**: introduce an internal `ExecTarget` /
   `ExecutionEnvironment` abstraction behind the existing `run_bash_core`
   (`tools.rs:1723`) and `execute_bash` (`tools.rs:2571-2589`) entry points; `Local`
   delegates to today's brush/subprocess/sandbox code unchanged. Zero behavior change;
   all existing bash/brush/sandbox tests must pass untouched.
2. **SSH transport for bash**: `Settings.execution.environments` with `kind = "ssh"`;
   `execute_bash` routes `exec` through `ssh user@host` with the same bounded-output,
   timeout, and cancellation contracts (cancellation kills the remote process group).
   Filesystem tooling stays local.
3. **Runner daemon + tool routing**: `rpi runner --stdio`/`--listen`; handshake +
   capability negotiation; `exec`/`cancel`; then `read`/`write`/`ls` routing so a remote
   cwd is fully workable; secrets handling and path containment per Section 4.3.
4. **Orchestration subagents on remote environments**: children inherit the session
   environment (`session.rs:1368-1423` plumbing); local sandboxed children unchanged;
   `hub read_history` and artifacts remain on the local side (transcripts never leave the
   agent host).

---

## 5. Acceptance criteria, risks, and invariants

### 5.1 Phase 1 — executable acceptance criteria

1. `cargo +1.88.0 check --workspace` passes with the persona module in place.
2. `cargo +1.88.0 test -p pi-coding --lib` and `cargo +1.88.0 test -p pi-cli --lib` pass,
   including the new persona unit tests; existing `role_contracts`,
   `durable_child_sessions`, `durable_orchestration_e2e`, and `hub_read_history_e2e`
   suites (`crates/pi-coding/tests/`) pass unchanged.
3. Create `<agent_dir>/personas/reviewer/persona.md` with frontmatter (name, description,
   tools, model, thinkingLevel, maxTurns, timeoutSecs, capabilityCeiling, softBudget) →
   `/persona` lists it and `/persona reviewer` shows its contract details
   (`interactive_commands.rs:296-320` surface).
4. `task { agent: "reviewer", task: "..." }` spawns a job whose `agent` is `reviewer`
   (`jobs.rs:63-91` snapshot); the child's memory namespace is
   `<agent_dir>/personas/reviewer/memory/entries.jsonl` (asserted by a unit test using a
   temp agent dir, `memory.rs:118-136` pattern).
5. Two sequential persona runs: run 1 calls `retain` (or `learn`); run 2's `recall`
   returns the entry — proven by a test that drives two spawns through the runtime with
   the same persona archive and asserts recall hits (`memory.rs:79-83` bounds respected).
6. `hub read_history { agentId: <persona run id> }` returns a bounded, redacted
   transcript for a *settled* run read from the persona archive (extension of the
   `read_history` sniffing path, `runtime.rs:4733-4737`); assert 200-line/32 KiB caps
   hold (`runtime.rs:37-43`).
7. A persona with `maxTurns: 2` yields with the role-contract stop reason
   (`runtime.rs:2947-3015`); a persona with a soft budget settles `Completed` with
   `soft_budget_exhausted: true` (`jobs.rs:63-91`).
8. Process restart durability: with a durable binding in place
   (`persistence.rs:144-185`), an interrupted persona run is parked (not lost) and the
   archive remains readable after restart.
9. `/persona reviewer --select` makes an unnamed `task` spawn resolve to `reviewer`
   (same preference path as `/role --select`, `interactive_commands.rs:196-203`).
10. Project-scope persona `<cwd>/.pi/personas/<name>/` loads only when the project is
    trusted (mirrors `definitions.rs:144-148`); untrusted project → persona absent with a
    diagnostic.

### 5.2 Phase 2 — executable acceptance criteria

1. Exec-boundary refactor: `cargo +1.88.0 test -p pi-coding --test brush_bash_e2e`,
   `--test sandbox_smoke`, `--test process_tool`, `--test process_manager`, plus the lib
   suites pass with zero behavioral diffs (proves the refactor is invisible).
2. Settings: `execution.environments` with `kind: "ssh"` validates; `kind: "docker"` (or
   any unknown kind) fails load with an actionable error; `allowed = false` environments
   are refused at selection.
3. SSH env: `/environment lab-box` then `bash` tool `hostname` returns the remote
   hostname; a `sleep 300` command is cancelled within the tool timeout (remote process
   group killed); output > 1 MiB is truncated and spilled exactly like local
   (`tools/bash/mod.rs:15-102`).
4. Runner daemon: `rpi runner --stdio` handshake + `exec` + `cancel` round-trip is
   covered by a test that spawns the runner as a child process; `--listen` daemon with
   `--secret-file` rejects a request with the wrong secret; `runnerUrl` plaintext
   non-loopback is rejected at settings validation.
5. Path containment: a `read`/`write` outside `allowedRoots` on the runner returns a
   fail-closed error (unit test against a fake runner or the real runner on loopback).
6. Remote + sandbox conflict: a bash call with both `environment: "lab-box"` and
   `sandboxed: true` fails validation, not silently dropping one.
7. Orchestration: with the session environment set to a remote env, a `task` child's
   bash runs remotely (`orchestration.sandboxed` stays local-only, `session.rs:1382-1423`);
   a locally-sandboxed child (default) still runs in `unshare`
   (`sandbox.rs:33-113`).
8. `cargo +1.88.0 check --workspace`, `git diff --check`, and the E2E gate from AGENTS.md
   (goal_loop_e2e, workflow_full_e2e, trust_hook_wiring, handoff_prose,
   plugin_marketplace_e2e, rewind_checkpoint_snapcompact_e2e, debug_tool_dap_e2e,
   sandbox_smoke) pass.

### 5.3 Risks / unknowns

- **SSH cancellation semantics**: killing a remote process tree over SSH requires
  process-group handling on the far side; the runner daemon path (phase 4.3) is the
  reliable cancellation story, SSH is best-effort — document the difference.
- **Latency/streaming**: remote exec has inherent latency; the bounded-chunk streaming
  must keep the existing tool-card progress contract (`tool_presentation.rs`).
- **Non-Linux hosts**: brush already falls back off-Linux (`brush.rs:31-36`); remote
  environments are inherently cross-platform but path containment assumes POSIX-ish
  paths — Windows runner support is explicitly out of scope initially.
- **Persona memory growth/privacy**: persona archives contain project data; retention
  (session TTL interplay) and purge semantics need a decision before GA (open question
  3.6b).
- **Name collisions**: a persona named `task` must be rejected (bundled agent, `definitions.rs:502-508`).
- **Runner distribution**: `rpi runner` is a new binary — needs install/update coverage
  (`install.sh`, `rpi update` surface, `args.rs:404-427`).

### 5.4 What must NOT change

- **Session isolation** (AGENTS.md invariant #1): workflows, memory, and transcript state
  stay scoped per session; persona archives are a new, separately-scoped namespace, never
  shared into other sessions' state.
- **Fail-closed guards** (invariant #2): the durable-recording binding
  (`persistence.rs:24-31`), the auth resolver, and trust decisions are untouched; personas
  and remote environments add no fallback paths that weaken them.
- **Trust never weakens** (invariant #3): persona/remote capabilities are additive and
  gated by the same approval/trust machinery; a stored denial can never be upgraded.
- **Bounded everything at the UI boundary** (invariant #5): remote outputs flow through
  the same truncation/redaction/spill pipeline as local output.
- **Local default**: with no `execution.environments` configured, every path behaves
  exactly as at HEAD `8261198` — brush, sandbox, `orchestration.sandboxed`, process
  extensions, and memory namespaces are unchanged.
