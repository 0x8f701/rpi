# Changelog

All notable changes to the `pi-rs` project are documented in this file.

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
  `grep`, `find`, and `ls` tools are available to custom sessions.
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

[0.1.0]: https://github.com/0x8f701/pi-rs/releases/tag/v0.1.0
