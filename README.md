# rpi (Rust port of pi)

`rpi` is a Rust port of the pi coding agent. It provides a self-contained CLI
for interactive and non-interactive AI-assisted coding, with pluggable
provider support, automatic context compaction, and native Pi v3 session
storage.

The executable binary is `rpi`. Runtime configuration still uses the upstream
pi layout (`~/.pi/agent`, project `.pi/`, and `PI_*` environment variables).
The companion headless binary remains `pi-rpc`.

## Install

macOS / Linux:

```sh
curl -fsSL https://raw.githubusercontent.com/0x8f701/pi-rs/master/install.sh | sh
```

Windows (PowerShell):

```powershell
irm https://raw.githubusercontent.com/0x8f701/pi-rs/master/install.ps1 | iex
```

Pin a release:

```sh
curl -fsSL https://raw.githubusercontent.com/0x8f701/pi-rs/master/install.sh | bash -s -- --version v0.2.0
```

Build and install from source:

```sh
cargo install --path crates/pi-cli --locked --bin rpi
```

The one-line installer places the active binary at `~/.pi-rs/bin/rpi`
(`%USERPROFILE%\.pi-rs\bin\rpi.exe` on Windows). See
[`docs/install.md`](docs/install.md) for manual builds, verification, and
platform requirements.

## Quick start

```sh
# Non-interactive print mode
rpi --print -m openai/gpt-5.5 "List the Rust files in this directory"

# JSON event stream (headless, one-shot)
rpi --mode json -m anthropic/claude-sonnet-4-5 "List Rust files"

# Interactive session (TUI if the terminal supports it, otherwise line REPL)
rpi -m anthropic/claude-sonnet-4-5

# List available models
rpi models

# Continue the most recent session for the current directory
rpi --continue
```

See [`docs/quickstart.md`](docs/quickstart.md) for a walkthrough and
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
| Default coding tools (`read`, `bash`, `edit`, `write`); optional `grep`, `find`, `ls` tools | Implemented |
| Native Pi v3 session storage, resume, import, export, and share | Implemented |
| Built-in model catalog + custom models via `models.json` | Implemented |
| Authentication via env vars, `auth.json`, `models.json`, `rpi login`/`logout` | Implemented |
| Provider streaming for OpenAI, Anthropic, Google, and OpenAI Responses | Implemented |
| Faux provider for tests and examples | Implemented |
| Automatic context compaction | Implemented |
| `AGENTS.md` / `CLAUDE.md` project context and `.pi/skills` | Implemented |
| Local/self-hosted models via `rpi llama` + llama.cpp router | Implemented |
| JSON-RPC / stdio server (`--mode rpc` and `pi-rpc`) | Implemented |
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
- `rpi update [--self|--extensions] [PACKAGE]` — update rpi or configured packages
- `rpi llama configure|status|refresh|load|unload|search|details|download|installed` — manage local models

See [`docs/cli-modes.md`](docs/cli-modes.md) for details.

## Changelog

See [`CHANGELOG.md`](CHANGELOG.md).

## License

MIT — see [`LICENSE`](LICENSE).
