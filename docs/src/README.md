# rpi

rpi is a Rust coding agent with an interactive terminal interface, noninteractive text/JSON output, a JSONL control protocol, a browser client, and Agent Client Protocol support for editors. It stores native conversations as Pi v3 JSONL.

Start with [installation](introduction/install.md) and [quick start](introduction/quickstart.md). The existing chapter order separates operator workflows from configuration, tool, protocol, and security references.

## Main capabilities

- Model selection and provider authentication, with configuration profiles and credential scopes.
- Workspace-scoped file tools, structural search/editing, supervised processes, and governed browser/service tools.
- Goals, Todo dependency execution, child coding jobs, peer messages, and isolated workflows.
- Recurring prompt loops through the terminal and Web Loop panel.
- Session resume, foreign-session import, rewind/checkpoints, compaction, handoff, and local export.
- Web Source/Preview file surfaces, process output viewing, token pairing, and safe rich-text rendering.

The active tool set depends on settings, resources, trust, and approval. A capability listed here is not a promise that it is enabled in every session or supported by every platform.

## Current contracts

Search defaults are grep 1,000 matches, ls/find 5,000 entries/results, and glob 200. Web directory and changed-file catalogs are bounded at 5,000. Recurring-loop presets are 30s, 1m, 5m, 15m, 1h, and 4h.

Imports use agent-loader v0.7.0 for Pi, OMP, Codex, Claude, Grok, and Droid sessions. See [session recovery](user-guide/session-recovery.md) for OMP rotation-chain semantics.

**Steering queue status: in progress.** Web previews are implemented; Rust scoped cancellation integration remains in flight. This book does not present it as a stable end-to-end feature.

## Security first

Project trust, tool approval, process confinement, and safe Markdown rendering protect different boundaries. Read [Security](reference/security.md) before enabling remote listeners, admitting external extensions, or executing unfamiliar project code. Never publish provider credentials or token-bearing pairing URLs.
