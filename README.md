# rpi

`rpi` is a self-contained Rust coding agent CLI for interactive and
non-interactive AI-assisted coding, with pluggable
provider support, automatic context compaction, and native Pi v3 session
storage.

The executable binary is `rpi`. Runtime configuration remains compatible with
the upstream Pi layout (`~/.pi/agent`, project `.pi/`, and `PI_*` environment variables).
The companion headless binary is `rpi-rpc`.

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

Pin the installer and release to `v0.2.3`:

```sh
curl -fsSL https://raw.githubusercontent.com/0x8f701/rpi/v0.2.3/install.sh | bash -s -- --version v0.2.3
```

The release archive contains the compiled `rpi` executable; users do not need
Rust or a source checkout. Maintainers who need a local source build can follow
the explicitly separated developer fallback in [`docs/install.md`](docs/install.md).

The release installer places the active binary at `~/.rpi/bin/rpi`
(`%USERPROFILE%\.rpi\bin\rpi.exe` on Windows) and adds that directory to
the user `PATH` when needed. Open a new terminal before running `rpi` if the
installer reports that it changed `PATH`. See [`docs/install.md`](docs/install.md)
for supported platforms, manual verification, and rollback behavior.

## Quick start

Configure one provider before the first model request:

```sh
rpi login anthropic
# Or set an environment variable instead:
export ANTHROPIC_API_KEY="<your-anthropic-key>"
```

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

See [`docs/quickstart.md`](docs/quickstart.md) for the first-run walkthrough and
[`docs/cli-modes.md`](docs/cli-modes.md) for every flag and subcommand.

## Documentation

- [`docs/install.md`](docs/install.md) — installation, platforms, and verification
- [`docs/quickstart.md`](docs/quickstart.md) — first steps
- [`docs/cli-modes.md`](docs/cli-modes.md) — print mode, REPL, TUI, slash commands
- [`docs/settings-trust.md`](docs/settings-trust.md) — `settings.json`, config directory, and trust boundaries
- [`docs/authentication.md`](docs/authentication.md) — env vars, `auth.json`, `models.json`, and precedence
- [`docs/models.md`](docs/models.md) — model catalog, model spec syntax, and custom providers
- [`docs/rpc-json.md`](docs/rpc-json.md) — event schema for library consumers
- [`docs/tui.md`](docs/tui.md) — TUI keybindings and status bar
- [`docs/prompt-templates.md`](docs/prompt-templates.md) — system prompt assembly
- [`docs/skills.md`](docs/skills.md) — `.pi/skills` discovery
- [`docs/update.md`](docs/update.md) — release and update safety
- [`docs/export-share.md`](docs/export-share.md) — session export, clipboard, and gist sharing
- [`docs/local-llama.md`](docs/local-llama.md) — local/self-hosted models
- [`docs/extensions.md`](docs/extensions.md) — process extension protocol and UI requests
- [`docs/packages.md`](docs/packages.md) — local/git packages and the explicitly unsupported npm backend
- [`docs/security.md`](docs/security.md) — credentials, path scoping, and installer safety
- [`docs/environment-variables.md`](docs/environment-variables.md) — all environment variables

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
| JSON-RPC / stdio server (`--mode rpc` and `rpi-rpc`) | Implemented |
| Custom TUI themes and keybindings | Implemented |
| Local/git packages (`rpi install/remove/list/update`) | Implemented |
| Process extension protocol via `pi-extension.json` manifests | Implemented |
| npm package backend | Not implemented |

## Subcommands

- `rpi models [filter]` — list models
- `rpi sessions` — list sessions
- `rpi import-session SOURCE INPUT [--output PATH]` — convert external sessions
- `rpi export SESSION_PATH [--output PATH] [--jsonl]` — export to HTML/JSONL
- `rpi login [provider]` / `rpi logout [provider]` — manage stored credentials
- `rpi reload` — validate and print active resources
- `rpi install SOURCE [--local]` / `rpi remove SOURCE [--local]` / `rpi list` — manage local/git packages
- `rpi update [--self] [--extensions] [--all] [--models] [--extension SOURCE] [PACKAGE]` — update rpi, packages, or model catalogs
- `rpi llama configure|status|refresh|load|unload|search|details|download|installed` — manage local models

See [`docs/cli-modes.md`](docs/cli-modes.md) for details.

## Changelog

See [`CHANGELOG.md`](CHANGELOG.md).

## License

MIT — see [`LICENSE`](LICENSE).
