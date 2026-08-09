# rpi

`rpi` is a self-contained Rust coding agent CLI for interactive and
non-interactive AI-assisted coding, with pluggable
provider support, automatic context compaction, and native Pi v3 session
storage.

The executable binary is `rpi`. Runtime configuration remains compatible with
the upstream Pi layout (`~/.pi/agent`, project `.pi/`, and `PI_*` environment variables).
The headless JSONL RPC control plane is `rpi rpc` (≡ `--mode rpc`).

## Install

Install the prebuilt `rpi` binary from GitHub Releases. The installer selects
the current platform archive, verifies `SHA256SUMS`, and activates the binary:

```sh
curl -fsSL https://raw.githubusercontent.com/0x8f701/rpi/master/install.sh | sh
```

Windows (PowerShell):

```powershell
irm https://raw.githubusercontent.com/0x8f701/rpi/master/install.ps1 | iex
```

Pin both the installer source and selected release to `v0.2.5`:

```sh
curl -fsSL https://raw.githubusercontent.com/0x8f701/rpi/v0.2.5/install.sh | bash -s -- --version v0.2.5
```

```powershell
& ([scriptblock]::Create((irm https://raw.githubusercontent.com/0x8f701/rpi/v0.2.5/install.ps1))) -Version v0.2.5
```

The release archive contains the compiled `rpi` executable; users do not need
Rust or a source checkout. Maintainers who need a local source build can follow
the explicitly separated developer fallback in [`docs/src/introduction/install.md`](docs/src/introduction/install.md).

The release installer places the active binary at `~/.rpi/bin/rpi`
(`%USERPROFILE%\.rpi\bin\rpi.exe` on Windows) and adds that directory to
the user `PATH` when needed. Open a new terminal before running `rpi` if the
installer reports that it changed `PATH`. See [`docs/src/introduction/install.md`](docs/src/introduction/install.md)
for supported platforms, manual verification, and rollback behavior.

## Quick start

Configure one provider before the first model request:

```sh
rpi login anthropic
```

For non-interactive setup, set the provider-specific environment variable to a
redacted credential value (see [`docs/src/user-guide/authentication.md`](docs/src/user-guide/authentication.md)).

Then run:

```sh
# Non-interactive print mode
rpi --print -m anthropic/claude-sonnet-4-5 "List the Rust files in this directory"

# JSON event stream (headless, one-shot)
rpi --mode json -m anthropic/claude-sonnet-4-5 "List Rust files"

# Interactive inline TUI (or line REPL when no terminal is available)
rpi -m anthropic/claude-sonnet-4-5

# List available models
rpi models

# Continue the newest saved session for the current directory
rpi --continue
```

See [`docs/src/introduction/quickstart.md`](docs/src/introduction/quickstart.md) for the first-run walkthrough and
[`docs/src/user-guide/cli-modes.md`](docs/src/user-guide/cli-modes.md) for every flag and subcommand.

## Documentation

- [`docs/src/introduction/install.md`](docs/src/introduction/install.md) — installation, platforms, and verification
- [`docs/src/introduction/quickstart.md`](docs/src/introduction/quickstart.md) — first steps
- [`docs/src/user-guide/cli-modes.md`](docs/src/user-guide/cli-modes.md) — print mode, REPL, TUI, slash commands
- [`docs/src/reference/settings-trust.md`](docs/src/reference/settings-trust.md) — `settings.json`, config directory, and trust boundaries
- [`docs/src/reference/architecture.md`](docs/src/reference/architecture.md) — crate dependency and runtime architecture diagrams
- [`docs/src/user-guide/authentication.md`](docs/src/user-guide/authentication.md) — env vars, `auth.json`, `models.json`, and precedence
- [`docs/src/user-guide/models.md`](docs/src/user-guide/models.md) — model catalog, model spec syntax, and custom providers
- [`docs/src/user-guide/rpc-json.md`](docs/src/user-guide/rpc-json.md) — event schema for library consumers
- [`docs/src/user-guide/tui.md`](docs/src/user-guide/tui.md) — TUI keybindings and status bar
- [`docs/src/reference/prompt-templates.md`](docs/src/reference/prompt-templates.md) — system prompt assembly
- [`docs/src/reference/skills.md`](docs/src/reference/skills.md) — `.pi/skills` discovery
- [`docs/src/reference/update.md`](docs/src/reference/update.md) — release and update safety
- [`docs/src/reference/export-share.md`](docs/src/reference/export-share.md) — session export, clipboard, and gist sharing
- [`docs/src/reference/local-llama.md`](docs/src/reference/local-llama.md) — local/self-hosted models
- [`docs/src/reference/extensions.md`](docs/src/reference/extensions.md) — process extension protocol and UI requests
- [`docs/src/reference/packages.md`](docs/src/reference/packages.md) — local/git packages for `rpi install` (npm package sources deferred; the plugin marketplace accepts `npm:` sources via `rpi plugin install`)
- [`docs/src/reference/security.md`](docs/src/reference/security.md) — credentials, path scoping, and installer safety
- [`docs/src/reference/environment-variables.md`](docs/src/reference/environment-variables.md) — all environment variables
- [`docs/src/user-guide/goals.md`](docs/src/user-guide/goals.md) — durable session goals: lifecycle, token budget, pins, journal
- [`docs/src/user-guide/todos.md`](docs/src/user-guide/todos.md) — the Todo DAG, `/todo` panel, and steering/follow-up queues
- [`docs/src/user-guide/orchestration.md`](docs/src/user-guide/orchestration.md) — subagents, jobs, soft budgets, IRC, and the `task`/`hub`/`yield` tools
- [`docs/src/user-guide/workflows.md`](docs/src/user-guide/workflows.md) — isolated concurrent workflows (worktree/overlayfs/none) with planning and Todo-DAG execution
- [`docs/src/user-guide/session-recovery.md`](docs/src/user-guide/session-recovery.md) — `/rewind`, `/checkpoint`, `/snapcompact`, `/handoff`, doom-loop recovery, and session TTL
- [`docs/src/user-guide/live.md`](docs/src/user-guide/live.md) — hold-to-talk voice input (`/live`)
- [`docs/src/reference/configuration-profiles.md`](docs/src/reference/configuration-profiles.md) — `--profile`, TOML settings, env expansion, and scoped auth
- [`docs/src/reference/sandbox-isolation.md`](docs/src/reference/sandbox-isolation.md) — Linux filesystem sandbox and overlayfs isolation
- [`docs/src/reference/hooks.md`](docs/src/reference/hooks.md) — host hooks and the trust hook
- [`docs/src/reference/memory.md`](docs/src/reference/memory.md) — local and Hindsight memory backends
- [`docs/src/reference/tools.md`](docs/src/reference/tools.md) — extended tool catalog (LSP, browser, GitHub, debug, eval, notebook, images, ask)
- [`docs/src/reference/mcp.md`](docs/src/reference/mcp.md) — Model Context Protocol client
- [`docs/src/reference/acp.md`](docs/src/reference/acp.md) — Agent Client Protocol mode (`rpi agent stdio`/`serve`)
- [`docs/src/user-guide/e2e-scenarios.md`](docs/src/user-guide/e2e-scenarios.md) — user-perspective end-to-end scenarios (tmux-driven)

Runnable examples are in [`examples/`](examples/).

## What is implemented in this release

| Area | Status |
|------|--------|
| Print mode, line REPL, TUI, JSON/RPC headless modes | Implemented |
| Default coding tools (`read`, `bash`, `edit`, `write`); optional `grep`, `find`, `glob`, `ls` tools | Implemented |
| Native Pi v3 session storage, resume, import, export, and share | Implemented |
| Built-in model catalog + custom models via `models.json` | Implemented |
| Authentication via env vars, `auth.json`, `models.json`, `rpi login`/`logout` | Implemented |
| Provider streaming for OpenAI, Anthropic, Google, and OpenAI Responses | Implemented |
| Faux provider for tests and examples | Implemented |
| Automatic context compaction | Implemented |
| `AGENTS.md` / `CLAUDE.md` project context and `.pi/skills` | Implemented |
| Local/self-hosted models via `rpi llama` + llama.cpp router | Implemented |
| JSON-RPC / stdio server (`--mode rpc` and `rpi rpc`) | Implemented |
| Custom TUI themes and keybindings | Implemented |
| Local/git packages (`rpi install/remove/list/update`) | Implemented |
| Process extension protocol via `pi-extension.json` manifests | Implemented |
| Plugin marketplace (`rpi plugin install/list/remove/update`; sources: directory, archive, GitHub `owner/repo`, `npm:<name>[@<version>]` with sha512 `dist.integrity` verification) | Implemented |
| npm package sources for the `rpi install` package manager | Not implemented (deferred) |
| Durable session goals (`/goal`: lifecycle, token budget, pins, journal) | Implemented |
| Todo DAG (`todo` tool, `/todo` panel, dependency execution) | Implemented |
| Orchestration: subagents, jobs, soft budgets, IRC, `task`/`hub`/`yield` tools | Implemented |
| Isolated concurrent workflows (`/workflow`: worktree/overlayfs/none isolation, planning + Todo-DAG execution) | Implemented |
| Session recovery (`/rewind`, `/checkpoint`, `/snapcompact`, `/handoff`) and startup session TTL pruning | Implemented |
| Hold-to-talk voice input (`/live`) | Implemented (requires `live-capture` build feature) |
| Linux filesystem sandbox (`sandbox` settings) and overlayfs isolation | Implemented (Linux) |
| Model Context Protocol (MCP) client (`mcpServers` + `mcp` tool) | Implemented (stdio transport) |
| Agent Client Protocol (ACP) mode (`rpi agent stdio` / `rpi agent serve`) | Implemented |
| Host hooks (`settings.hooks`) and trust hooks | Implemented |
| Memory backends (local JSONL; `recall`/`retain`/`reflect` via configured Hindsight HTTP API) | Implemented |
| Extended tools: LSP, headless browser, GitHub, DAP debug, eval, notebook, image generation/inspection | Implemented |

## Subcommands

- `rpi models [filter]` — list models
- `rpi sessions` — list sessions
- `rpi import-session SOURCE INPUT [--output PATH]` — convert external sessions
- `rpi export SESSION_PATH [--output PATH] [--jsonl]` — export to HTML/JSONL
- `rpi login [provider]` / `rpi logout [provider]` — manage stored credentials
- `rpi reload` — validate and print active resources
- `rpi install SOURCE [--local]` / `rpi remove SOURCE [--local]` / `rpi list` — manage local/git packages
- `rpi plugin list [--updates]` / `rpi plugin install SOURCE` / `rpi plugin remove NAME` / `rpi plugin update NAME` — manage marketplace plugins
- `rpi update [--self] [--extensions] [--all] [--models] [--extension SOURCE] [PACKAGE]` — update rpi, packages, or model catalogs
- `rpi llama configure|status|refresh|load|unload|search|details|download|installed` — manage local models
- `rpi agent stdio` / `rpi agent serve` — Agent Client Protocol (ACP) mode for ACP-speaking editors

See [`docs/src/user-guide/cli-modes.md`](docs/src/user-guide/cli-modes.md) for details.

## Web control plane

`rpi --listen` serves the web client at `/web` on the control-plane listener.
The listener defaults to loopback and authentication is optional.

**Local, no token** — the default, one command:

```console
$ rpi --listen 127.0.0.1:8765
```

Open <http://127.0.0.1:8765/web>; the browser auto-connects with no token.

**LAN, no token** — bind a non-loopback address and explicitly opt into
plaintext remote listening (still no token):

```console
$ rpi --listen 0.0.0.0:8765 --listen-allow-insecure-remote
```

Open `http://<host-lan-ip>:8765/web` (or any hostname that routes to the
host) from another machine; the browser auto-connects with no token. No
`--listen-advertised-origin` is needed for ordinary `/web`, `/ws`, or
`/rpc`: the browser request is accepted when its `Origin` authority equals
the HTTP `Host` — an ordinary same-origin check that rejects unrelated
cross-origin pages, not authentication and not DNS-rebinding protection.
This is **unauthenticated and unencrypted**: anyone reachable on the
network can drive the agent and observe traffic. Prefer loopback or a
TLS-terminating proxy on untrusted networks.

**Authenticated** — add `--listen-token-file <path>` to any of the above to
make the token mandatory; the browser then requires it:

```console
$ rpi --listen 0.0.0.0:8765 --listen-token-file <workspace>/rpi-token \
      --listen-allow-insecure-remote
```

The token authenticates clients but does not encrypt the bearer token or
control traffic against passive LAN observers. `rpi agent serve` remains
loopback-only and rejects tokenless browsers.

Collaboration join links (`/collab`, `collab_start` without an explicit
`baseUrl`) cannot be synthesized from a wildcard bind. For a wildcard
`--listen` address (0.0.0.0 or `::`), pass
`--listen-advertised-origin <URL>` — a strict http/https origin with no
credentials, path, query, or fragment — so links point at a reachable host.
Loopback and other specific binds advertise their bound address automatically.

## Changelog

See [`CHANGELOG.md`](CHANGELOG.md).

## License

MIT — see [`LICENSE`](LICENSE).
