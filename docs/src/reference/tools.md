# Extended tool catalog

The active tool set is assembled from built-ins, configured capabilities, extensions, and orchestration. A name in this catalog does not mean it is enabled in every session. The model receives the active schema; use that schema for argument details.

## Files and search

| Tool | Purpose |
| --- | --- |
| `read` | Read file content |
| `write` | Create or overwrite a workspace-scoped file |
| `edit` | Exact-text replacement, including disjoint edits |
| `grep` | Content pattern search, respecting ignore rules |
| `find` | Filename pattern search, respecting ignore rules |
| `glob` | Bounded glob matching under workspace roots |
| `ls` | Directory listing |
| `ast_grep` | Structural code search with metavariables |
| `ast_edit` | Structural rewrite of one file |

Model-facing defaults are **grep 1,000 matches**, **ls 5,000 entries**, **find 5,000 results**, and **glob 200 results**. Glob also has a hard maximum of 200. General head/tail truncation defaults to 2,000 lines or 50 KiB, and grep output lines have a separate length bound. Result-count limits and output-byte limits are independent.

`--add-dir` extends scoped search and attachment roots. File mutation still uses retained filesystem authority and rejects external/symlink escapes. A path mentioned in a prompt does not add authority.

## Shell and processes

`bash` runs finite foreground commands and supports supervised long-lived commands through `background=true`. Foreground pseudo-terminal mode uses `pty=true` with optional supplied input and cannot be combined with background mode.

There is **no shell grep/rg block guard**. The generated prompt can prefer dedicated search tools, but bash accepts shell search. Unsupervised background/detached work remains prohibited: do not use shell `&`, `nohup`, `setsid`, or `disown` to evade supervision.

The `process` tool manages supervised processes; the terminal `/ps` and Web Processes panel expose their state and output. `hub` manages child coding jobs, not operating-system processes.

## Browsing and service tools

- `browser`: navigate, click, fill, screenshot, extract, list tabs, and close a governed Chrome/Chromium browser.
- `web_search`: Exa-backed search; disabled by `RPI_OFFLINE`.
- `github`: governed GitHub CLI or session HTTP operations for issues, pull requests, commits, files, and code search.
- `mcp`: configured server discovery and tool calls; shipped execution transport is stdio.
- `ask`: user questions in interactive sessions.

## Development tools

- `lsp`: language-server hover, definitions, references, diagnostics, symbols, rename, and code actions.
- `debug`: session-scoped debug-adapter control; one adapter at a time.
- `eval`: persistent session-scoped Python or embedded JavaScript evaluation with bounded, redacted output.
- `notebook`: read, execute, and edit notebook cells; execution output persists only when requested.

## Images and memory

`inspect_image` reads image metadata and statistics without rendering. `generate_image` uses the configured image endpoint and writes generated images into the workspace. It accepts bounded prompts and image counts; it is distinct from the active chat model unless configured to use it.

Local `memory` supports learn/recall/list/forget. The external Hindsight backend substitutes `recall`, `retain`, and `reflect`. `off` removes memory tools. See [memory](memory.md).

## Tool selection and safety

`--tools` applies an allowlist and `--exclude-tools` applies a denylist afterward. `--no-tools` removes all contributions; `--no-builtin-tools` retains non-built-in contributions. Tool registration does not bypass host approval, project trust, sandbox policy, or credential audiences.

The Web file explorer has separate 5,000-entry/change bounds; do not confuse them with model tool defaults. The frontend policy calls for generous limits, not unlimited output.
