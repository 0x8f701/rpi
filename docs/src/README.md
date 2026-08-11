# rpi — reference manual

`rpi` is a terminal coding agent for Pi v3 sessions: a single Rust binary that
switches between print mode, a line REPL, a full-screen inline TUI, and headless
JSON/RPC modes. It supports multiple model providers, local llama.cpp models,
extensions, skills, prompt templates, packages, session export and sharing, a
security model built around project trust and credential redaction, and a full
orchestration stack: durable goals, Todo DAGs, subagent jobs with IRC
coordination, isolated concurrent workflows, MCP/ACP protocol clients, voice
input, memory backends, and host hooks.

## Getting started

- [Quick start](introduction/quickstart.md) — first-run workflow
- [Installation](introduction/install.md) — supported platforms, installers, verification

## User guide

- [TUI and keybindings](user-guide/tui.md)
- [CLI modes and commands](user-guide/cli-modes.md)
- [Goals](user-guide/goals.md)
- [Todos and the Todo DAG](user-guide/todos.md)
- [Orchestration: subagents, jobs, and IRC](user-guide/orchestration.md)
- [Isolated concurrent workflows](user-guide/workflows.md)
- [Session recovery: rewind, checkpoints, handoffs, and TTL](user-guide/session-recovery.md)
- [Live voice: TUI `/live` STT and `/web` realtime](user-guide/live.md)
- [Models and custom providers](user-guide/models.md)
- [Authentication](user-guide/authentication.md)
- [RPC JSONL protocol](user-guide/rpc-json.md)
- [Web client (`/web`)](user-guide/web.md)
- [E2E scenarios (user-perspective tmux tests)](user-guide/e2e-scenarios.md)

## Reference

- [Settings, configuration, and trust](reference/settings-trust.md)
- [Configuration profiles, TOML settings, and scoped auth](reference/configuration-profiles.md)
- [Environment variables](reference/environment-variables.md)
- [Sandbox and overlayfs isolation](reference/sandbox-isolation.md)
- [Hooks and trust hooks](reference/hooks.md)
- [Extensions and process protocol](reference/extensions.md)
- [Skills](reference/skills.md)
- [Packages](reference/packages.md)
- [Memory](reference/memory.md)
- [Extended tool catalog](reference/tools.md)
- [Model Context Protocol (MCP) client](reference/mcp.md)
- [Agent Client Protocol (ACP) mode](reference/acp.md)
- [Prompt templates and system prompt assembly](reference/prompt-templates.md)
- [Local / self-hosted models with llama.cpp](reference/local-llama.md)
- [Security](reference/security.md)
- [Export and share](reference/export-share.md)
- [Update safety](reference/update.md)
