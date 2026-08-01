# CLI modes and commands

`rpi` is a single binary that switches between non-interactive, headless, and interactive modes based on the flags you pass.

## Top-level syntax

```text
rpi [OPTIONS] [PROMPT]...
```

When no subcommand is given, a top-level run is selected by the flags:

1. `--mode json` or `--mode rpc` → headless structured I/O.
2. `-p`/`--print` or a non-empty positional `[PROMPT]...` → print mode.
3. `stdout` is a terminal and raw mode works → TUI.
4. Otherwise → line REPL.

Multiple positional `[PROMPT]` arguments are joined with a single space. An empty joined prompt (for example `rpi ""`) does **not** force print mode and enters the interactive session instead, matching the upstream behavior.

## Top-level flags

| Flag | Short | Value | Meaning |
|------|-------|-------|---------|
| `--model <SPEC>` | `-m` | e.g. `anthropic/claude-sonnet-4-5` | Model to use. |
| `--provider <PROVIDER>` | | provider id | Provider used with `--model`. |
| `--models <PATTERNS>` | | comma-separated patterns | Scope interactive model cycling. |
| `--print` | `-p` | | Force print mode. |
| `--mode <text\|json\|rpc>` | | protocol | Headless output protocol. `text` is the default interactive run; `json` streams one-shot events; `rpc` reads JSONL commands on stdin. |
| `--continue` | `-c` | | Resume the most recent session for this directory. |
| `--resume <PATH_OR_ID>` | | path or id | Resume a native Pi session or import and resume a discovered OMP, Codex, Claude, Grok, or Droid session. Exact ids and unambiguous prefixes are accepted. |
| `--session <PATH_OR_ID>` | | path or id | Open a session by file path, exact id, or unambiguous prefix. |
| `--session-id <ID>` | | exact id | Open an exact project session id, creating it when absent. |
| `--fork <PATH_OR_ID>` | | path or id | Fork a session by file path, exact id, or unambiguous prefix. |
| `--session-dir <DIR>` | | directory | Override the directory used for session storage and id lookup. |
| `--no-session` | | | Do not persist a session file for this run. |
| `--name <NAME>` | `-n` | display name | Set the session display name. |
| `--system-prompt <TEXT_OR_PATH>` | `--system` | text or file | Override the system prompt with text or an existing file. |
| `--append-system-prompt <TEXT_OR_PATH>` | | text or file | Append to the system prompt; repeatable. |
| `--cwd <DIR>` | `-C` | directory | Working directory for this run and global subcommands. |
| `--thinking <LEVEL>` | `--think` | `off\|minimal\|low\|medium\|high\|xhigh\|max` | Initial reasoning level. |
| `--api-key <KEY>` | | secret | Override the API key for the resolved model's provider. |
| `--tools <TOOLS>` | `-t` | comma-separated names | Allowlist applied after tool assembly. |
| `--exclude-tools <TOOLS>` | `-xt` | comma-separated names | Denylist applied after the allowlist. |
| `--no-tools` | `-nt` | | Disable all built-in, extension, orchestration, and custom tools. |
| `--no-builtin-tools` | `-nbt` | | Disable built-in tools while preserving others. |
| `--extensions <PATH>` | `-e` | file or directory | Load an explicit extension manifest; repeatable. |
| `--no-extensions` | `-ne` | | Disable discovered/configured extensions while retaining explicit `--extensions` paths. |
| `--skill <PATH>` | | file or directory | Load an explicit skill; repeatable. |
| `--no-skills` | `-ns` | | Disable discovered/configured skills while retaining explicit `--skill` paths. |
| `--prompt-template <PATH>` | | file or directory | Load an explicit prompt template; repeatable. |
| `--no-prompt-templates` | `-np` | | Disable discovered/configured prompt templates while retaining explicit paths. |
| `--theme <PATH>` | | file or directory | Load an explicit theme; repeatable. |
| `--no-themes` | | | Disable discovered/configured themes while retaining explicit paths. |
| `--no-context-files` | `-nc` | | Disable `AGENTS.md` and `CLAUDE.md` discovery. |
| `--list-models [SEARCH]` | | optional filter | List models and exit. |
| `--offline` | | | Disable nonessential startup networking such as catalog refreshes and update checks. |
| `--verbose` | | | Force verbose startup diagnostics. |
| `--approve` | `-a` | | Trust project-local `.pi` settings/resources for this run only. |
| `--no-approve` | | | Refuse project-local `.pi` settings/resources for this run only. |
| `--version` | `-v`, `-V` | | Print version and exit. |
| `--help` | `-h` | | Print help and exit. |

Short aliases are normalized before clap parses: `-v` maps to `--version`, `-xt` to `--exclude-tools`, `-nt` to `--no-tools`, `-nbt` to `--no-builtin-tools`, `-ne` to `--no-extensions`, `-ns` to `--no-skills`, `-np` to `--no-prompt-templates`, and `-nc` to `--no-context-files`.

`--provider` requires `--model`; `--api-key` requires `--model` or `--models`. Resume/session/fork/no-session flags are mutually exclusive.

## Subcommands

### `rpi models [FILTER]`

List available models. Provider headers are printed in bold. The filter is case-sensitive and matches against provider name or model id.

### `rpi sessions`

List native Pi v3 sessions for the configured working directory, newest first. Honors the global `-C` / `--cwd` flag in any position:

```sh
rpi --cwd /path sessions
rpi sessions -C /path
```

### `rpi import-session <SOURCE> <INPUT> [--output PATH]`

Convert an external session to native Pi v3 JSONL.

Supported `SOURCE` values:

- `pi`
- `omp`
- `codex`
- `claude`
- `grok`
- `droid`

With `--output` the file is written to that path (or into the directory if the path is an existing directory). Without `--output` the file is placed under the per-cwd session directory and the command prints the emitted path and message count.

### `rpi login [PROVIDER]` / `rpi logout [PROVIDER]`

Configure or remove stored credentials in `auth.json`. When run in an interactive terminal with no provider, a list of configured providers is shown. Outside a terminal the provider argument is required.

### `rpi reload`

Validate the active settings/resource snapshot and print a JSON summary to stdout.

### `rpi export <SESSION_PATH> [--output PATH] [--jsonl]`

Export a native Pi v3 session file to a self-contained HTML file (default) or to a current-branch JSONL file with `--jsonl`. No model, auth, or network access is required. Prints the output path to stdout.

### `rpi install <SOURCE> [--local]` / `rpi remove <SOURCE> [--local]` / `rpi uninstall <SOURCE> [--local]` / `rpi list`

Install, remove, or list local/git Pi packages. `--local` persists the package in the project's `.pi/settings.json` instead of the global agent settings. Project packages are only loaded when the project is trusted.

### `rpi config [-l|--local]`

Configure enabled package resources (extensions, skills, prompts, themes) for global or project scope. In a terminal it opens an interactive selector; with non-TTY stdout it prints deterministic JSON. Project scope is refused unless the project is trusted.

### `rpi update [--self|--extensions] [PACKAGE]`

Update the managed installation or configured extensions.

- no arguments or `--self`: update the managed `rpi` installation from GitHub releases.
- `--extensions`: reconcile every configured package (git refs and local paths).
- `--self --extensions`: update packages, then update `rpi`.
- positional `PACKAGE`: update one configured package by source identity.
- `--force` (with `--self`): reinstall even when version and checksum match.

`--self`, `--extensions`, and `PACKAGE` are mutually exclusive; `--force` is only valid with `--self`.

### `rpi llama <COMMAND>`

Manage a llama.cpp router and local GGUF downloads:

| Subcommand | Purpose |
|------------|---------|
| `configure URL [--api-key KEY]` | Configure and validate a router |
| `status [--reload]` | Show router catalog |
| `refresh` | Refresh live models, fall back to cached catalog |
| `load MODEL` / `unload MODEL` | Load/unload a model |
| `search QUERY` | Search Hugging Face for GGUF repos |
| `details OWNER/REPO` | List quantizations and checksums |
| `download OWNER/REPO [-q QUANT]` | Download one quantization atomically |
| `installed` | List local downloads |

## Interactive modes

The CLI chooses between experiences automatically:

1. **Headless JSON/RPC** — when `--mode json` or `--mode rpc` is passed.
2. **Print mode** — when `-p`/`--print` is set or a positional prompt is given.
3. **TUI** — when `stdout` is a TTY and raw terminal mode can be entered.
4. **Line REPL** — fallback when the TUI cannot start.

All share the same session engine; only the input and rendering differ.

## Print mode output

Print mode streams assistant text and tool activity to stdout:

```text
· bash({command})
  └ ok

The result is...
```

A trailing newline is appended after the final assistant text.

## Common slash commands

Both the TUI and the line REPL accept these commands:

| Command | Description |
|---------|-------------|
| `/help` | Show available commands |
| `/settings` | Show settings (REPL) or open the settings panel (TUI) |
| `/model [spec]` | Switch model (keeps transcript) |
| `/models [filter]` | List available models |
| `/new` | Clear transcript and start a fresh recording |
| `/name [name]` | Set or show the session name |
| `/session` | Show current session info |
| `/sessions` | List saved sessions for this directory |
| `/resume [path\|id\|prefix]` | List or resume native and discovered foreign sessions through the unified catalog |
| `/import <path.jsonl>` | Import and resume a native Pi v3 JSONL session |
| `/export [path]` | Export the session to HTML, or JSONL if the path ends in `.jsonl` |
| `/share` | Share the session via a private GitHub gist |
| `/copy` | Copy the last assistant message to the clipboard |
| `/fork [message-id]` | Fork from a previous user message |
| `/clone` | Clone the current active branch |
| `/tree` | Show or navigate the current session tree |
| `/loop [interval] <prompt>` | Run a prompt on a recurring interval |
| `/loops` | List active recurring loops |
| `/loop-update <id> [interval] [prompt]` | Update a loop interval, prompt, or both |
| `/loop-delete <id>` | Delete a loop without aborting an already-running turn |
| `/loop-cancel <id>` | Cancel a loop by id |
| `/compact [instructions]` | Manually compact session context |
| `/todo [markdown]` | Show or edit the task list |
| `/trust <trusted\|untrusted\|ask>` | Save a project trust decision |
| `/login [provider]` | Configure credentials |
| `/logout [provider]` | Remove stored credentials |
| `/llama [status\|configure\|refresh\|load\|unload]` | Manage the llama.cpp router |
| `/ps` | List supervised processes |
| `/process <start\|describe\|logs\|send\|resize\|signal\|stop\|wait> ...` | Control a supervised process |
| `/changelog` | Show version history |
| `/hotkeys` | Show keyboard shortcuts |
| `/quit` / `/exit` | Exit |

Loop intervals accept positive bare seconds (`/loop 300 check status`) or compact `s`, `m`, `h`, and `d` units (`/loop 3s echo hello`, `/loop 30m check deploy`). Values are honored exactly; zero and overflow are rejected.

## REPL-only additions

The line REPL also accepts:

| Command | Description |
|---------|-------------|
| `/think <level>` | Set reasoning level (`off`, `minimal`, `low`, `medium`, `high`, `xhigh`, `max`) |
| `!command` | Run a bash command recorded in context |
| `!!command` | Run a bash command excluded from context |

In the TUI, reasoning level and model cycling are controlled by keybindings instead. See [`docs/tui.md`](docs/tui.md).

## TUI-only slash commands

| Command | Description |
|---------|-------------|
| `/theme [name\|next\|prev]` | Show or switch the active theme |
| `/scoped-models` | Enable or disable models for cycling (opens TUI panel) |

## TUI keybindings

The TUI is a single full-screen panel with a conversation transcript on top, an input area on the bottom, and a status line.

| Key | Action |
|-----|--------|
| `Enter` | Submit the current message |
| `Shift+Enter` or `Ctrl+J` | Insert a newline |
| `Ctrl+D` (on empty input) | Quit |
| `Esc` | Cancel the in-flight run, or clear input when idle |
| `Ctrl+C` | Clear input |
| `Tab` | Accept the selected slash-command completion |
| `Up` / `Down` | Move cursor, or select a completion |
| `Left` / `Right` | Move cursor |
| `Backspace` / `Delete` | Edit text |
| `Home` / `End` | Move to start/end of line |
| `Ctrl+V` / `Alt+V` | Paste from clipboard |
| `Ctrl+X` / `Ctrl+Shift+C` | Copy last assistant message |
| `Ctrl+G` | Open external editor |
| `Ctrl+L` | Open model selector |
| `Ctrl+P` / `Ctrl+Shift+P` | Cycle model forward/backward |
| `Ctrl+T` | Toggle thinking level |
| `Shift+Tab` | Cycle thinking level |
| `Ctrl+O` | Expand tools panel |

Type `/hotkeys` in either interactive mode to see a shortcut summary. Default bindings can be overridden with custom keybinding JSON files. See [`docs/tui.md`](docs/tui.md).

## Session resume and import

Native sessions are Pi v3 JSONL files. Each line is an object with a `type` field. The active branch is reconstructed from `parentId` chains, so sessions can contain forks and the latest branch is followed on resume.

Session files live under:

```text
<agent-dir>/sessions/--<workspace>--/<timestamp>_<id>.jsonl
```

`<workspace>` is the encoded cwd: the absolute path with leading separators removed and `/`, `\`, and `:` replaced by `-`.

`--resume` and `/resume` share one catalog spanning native Pi plus OMP, Codex, Claude, Grok, and Droid homes. Native selections open the existing file without copying. Foreign selections are converted to Pi v3 once, retain source lineage, and reuse that native conversion on later resumes. Only convertible user/assistant text messages are preserved; tool calls, reasoning, attachments, and branches are dropped.

## Model spec syntax

- `provider/id` — explicit provider and model id.
- `id` — bare id matched across all providers.
- `provider/id:level` — explicit model plus thinking level suffix.
- `id:level` — bare id plus thinking level.

Resolution is case-insensitive. If the requested id is unknown but the provider is known, the CLI falls back to a synthetic custom-id model cloned from that provider's default template and prints a warning. This is useful with custom `models.json` entries.

## Cancellation

- In print mode, `Ctrl-C` aborts the in-flight turn and the CLI exits.
- In the REPL, `Ctrl-C` aborts the current turn and returns to the prompt.
- In the TUI, `Esc` aborts the in-flight run.

## Exit codes

- `0` — success.
- `1` — any error. Errors are printed to stderr without secret values.

## Related documentation

- [`authentication.md`](authentication.md) — env vars, `auth.json`, `models.json`
- [`models.md`](models.md) — model catalog, custom providers
- [`settings-trust.md`](settings-trust.md) — `settings.json`, config directory, trust
- [`tui.md`](tui.md) — TUI themes and keybindings
- [`rpc-json.md`](rpc-json.md) — JSON/RPC event schema
- [`local-llama.md`](local-llama.md) — local model setup
- [`packages.md`](packages.md) — package manager details
