# CLI modes and commands

`rpi` is a single binary that switches between non-interactive, headless, and interactive modes based on the flags you pass.

## Top-level syntax

```text
rpi [OPTIONS] [PROMPT]...
```

When no subcommand is given, a top-level run is selected by the flags and terminal:

1. `--mode json` or `--mode rpc` → headless structured I/O.
2. `-p` / `--print` → print mode.
3. A non-empty positional prompt with non-terminal stdin or stdout → print mode, unless `--listen` is active.
4. Both stdin and stdout are terminals → TUI, with positional prompts submitted as initial turns.
5. Otherwise → line REPL, with positional prompts submitted as initial turns. `--listen` keeps non-terminal prompt runs on this live REPL path so the listener shares the running `Application` until EOF.

Multiple positional `[PROMPT]` arguments are separate initial turns in JSON,
print, TUI, and REPL modes. An empty prompt is skipped by structured modes and
does not force print mode.

## Top-level flags

| Flag | Short | Value | Meaning |
|------|-------|-------|---------|
| `--model <SPEC>` | `-m` | e.g. `anthropic/claude-sonnet-4-5` | Model to use. |
| `--provider <PROVIDER>` | | provider id | Provider used with `--model`. |
| `--models <PATTERNS>` | | comma-separated patterns | Scope interactive model cycling. |
| `--print` | `-p` | | Force print mode. |
| `--mode <text\|json\|rpc>` | | protocol | Output protocol. `json` streams one-shot events; `rpc` reads LF-delimited JSON commands on stdin; `text` uses normal terminal selection. |
| `--continue` | `-c` | | Resume the newest native Pi session for this directory. Native-only; foreign sessions are never selected. |
| `--resume <PATH_OR_ID>` | `-r` | path or id | Resume a native Pi session or import and resume a discovered OMP, Codex, Claude, Grok, or Droid session. Exact ids and unambiguous prefixes are accepted. |
| `--session <PATH_OR_ID>` | | path or id | Open a session by file path, exact id, or unambiguous prefix. |
| `--session-id <ID>` | | exact id | Open an exact project session id, creating it when absent. |
| `--fork <PATH_OR_ID>` | | path or id | Fork a session by file path, exact id, or unambiguous prefix. |
| `--session-dir <DIR>` | | directory | Override the directory used for session storage and id lookup. |
| `--no-session` | | | Do not persist a session file for this run. |
| `--name <NAME>` | `-n` | display name | Set the session display name. |
| `--system-prompt <TEXT_OR_PATH>` | `--system` | text or file | Override the system prompt with text or an existing file. |
| `--append-system-prompt <TEXT_OR_PATH>` | | text or file | Append to the system prompt; repeatable. |
| `--cwd <DIR>` | `-C` | directory | Working directory for this run and global subcommands. |
| `--add-dir <DIR>` | | directory | Add a directory to scoped tools and `@file` expansion; repeatable. |
| `--thinking <LEVEL>` | `--think` | `off\|minimal\|low\|medium\|high\|xhigh\|max` | Initial reasoning level. |
| `--api-key <KEY>` | | secret | Override the API key for the resolved model's provider. |
| `--tools <TOOLS>` | `-t` | comma-separated names | Allowlist applied after tool assembly. |
| `--exclude-tools <TOOLS>` | `-xt` | comma-separated names | Denylist applied after the allowlist. |
| `--no-tools` | `-nt` | | Disable all built-in, extension, orchestration, and custom tools. |
| `--no-builtin-tools` | `-nbt` | | Disable built-in tools while preserving others. |
| `--extension <PATH>` | `-e` | file or directory | Load an explicit extension manifest; repeatable. `--extensions` is an alias. |
| `--no-extensions` | `-ne` | | Disable discovered/configured extensions while retaining explicit `--extension` paths. |
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
| `--approval-mode <MODE>` | | `yolo\|write\|ask` | Host tool approval policy. `yolo` allows all capabilities, `write` confirms Exec, and `ask` confirms every tool call. Non-interactive confirmation fails closed. This is separate from project trust flags. |
| `--listen <SOCKET_ADDR>` | | e.g. `127.0.0.1:8765` | Bind an opt-in HTTP/WebSocket control plane around the live TUI/REPL application. Only valid on the text path. |
| `--listen-token-file <PATH>` | | token file | Require exact Bearer authentication. Required for non-loopback listeners. Tokenless loopback accepts native clients without `Origin` and rejects browser-origin requests. |
| `--version` | `-v`, `-V` | | Print version and exit. |
| `--help` | `-h` | | Print help and exit. |

Short aliases are normalized before clap parses: `-v` maps to `--version`, `-xt` to `--exclude-tools`, `-nt` to `--no-tools`, `-nbt` to `--no-builtin-tools`, `-ne` to `--no-extensions`, `-ns` to `--no-skills`, `-np` to `--no-prompt-templates`, and `-nc` to `--no-context-files`.

`--provider` requires `--model`; `--api-key` requires `--model` or `--models`.
`--continue`, `--resume`, `--session`, `--session-id`, `--fork`, and `--no-session`
are mutually exclusive.

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

### `rpi install <SOURCE> [--local]` / `rpi remove <SOURCE> [--local]` / `rpi list`

Install, remove, or list local/git rpi packages. `--local` persists the package in the project's `.pi/settings.json` instead of the global agent settings. Project packages are only loaded when the project is trusted.

### `rpi config [-l|--local]`

Configure enabled package resources (extensions, skills, prompts, themes) for global or project scope. In a terminal it opens an interactive selector; with non-TTY stdout it prints deterministic JSON. Project scope is refused unless the project is trusted.

### `rpi update [OPTIONS] [PACKAGE]`

Update the managed installation, configured packages, or dynamic model catalogs.

- no arguments or `--self`: update the managed `rpi` installation.
- `--extensions`: reconcile every configured package.
- `--self --extensions` or `--all`: update packages, then update `rpi`.
- `--models`: refresh dynamic model catalogs.
- `--extension SOURCE` or positional `PACKAGE`: update one configured package.
- positional `self` or `rpi`: update the managed installation.
- `--force`: reinstall the selected self-update even when version and checksum match.

A package source cannot be combined with `--self`, `--extensions`, or `--force`.

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

1. **Headless JSON/RPC** — when `--mode json` or `--mode rpc` is passed.
2. **Print mode** — when `-p` / `--print` is set, or a positional prompt is
   supplied while stdin or stdout is not a terminal.
3. **TUI** — when both stdin and stdout are terminals.
4. **Line REPL** — fallback for non-terminal text sessions.

All share the same session engine; only the input and rendering differ.

## Print mode output

Print mode streams assistant text and tool activity to stdout:

```text
· bash({command})
  └ ok

The result is...
```

A trailing newline is appended after the final assistant text.

## Primary slash commands

`/help`, slash completion, and RPC command discovery expose exactly these 14
primary commands. Other built-ins remain manually executable where documented.

| Command | Description |
|---------|-------------|
| `/settings` | Inspect settings (REPL) or open the settings panel (TUI) |
| `/model [spec]` | Switch model without clearing the transcript |
| `/branch` | Create a new branch from a previous message |
| `/resume [path\|id\|prefix]` | List or resume native and discovered foreign sessions |
| `/fork` | Fork from a previous user message |
| `/export [path]` | Export the session to HTML or JSONL |
| `/agents` | Manage agent definitions and model overrides |
| `/compact [instructions]` | Manually compact session context |
| `/ps` | List supervised processes |
| `/loop [interval] <prompt>` | Run a prompt on a recurring interval |
| `/goal` | Create and manage the durable session goal |
| `/workflow` | Create and manage isolated concurrent workflows |
| `/code-review [<from> <to>]` | Browse a Git diff in a fullscreen tree/diff page: bare shows tracked HEAD→working-tree staged and unstaged changes; two refs compare any two commits/branches/tags |
| `/btw [prompt]` | Open a persistent detached side conversation forked from the active main branch |

Frequently used hidden built-ins include `/help`, `/new`, `/sessions`, `/tree`,
`/todo`, `/login`, `/logout`, `/share`, `/copy`, `/process`, `/theme`, and
`/quit`. They remain executable but do not appear in primary discovery.

`/btw` starts read-only. `Ctrl+T` toggles fresh workspace-scoped edit/exec tools when idle, `Esc` aborts a streaming side turn or closes an idle overlay, `Alt+R` reforks from the current main branch, and `Alt+N` clears the side transcript. Closing the overlay keeps its controller for reopening; leaving the TUI aborts and joins active side work. `peek_main` reads the current active main branch without writing to the main session.

`/code-review` uses direct bounded Git argv with pager, external diff, text conversion, configured filters, and interactive prompts disabled. It never sends repository content to a server. The page supports keyboard and mouse tree/hunk navigation, inline per-hunk comment threads after each complete hunk, fold/unfold controls, and one consolidated answer card per review exchange. `Esc` or `q` closes it and restores normal terminal mouse selection.

`/code-review` accepts zero or exactly two arguments. Bare `/code-review` shows
tracked HEAD→working-tree staged and unstaged changes. `/code-review <from> <to>`
resolves each ref — a commit hash, branch, or tag — to a commit and renders a
commit-to-commit diff labeled `<from> → <to>` in the panel title; working-tree
changes are ignored on this path. One argument or more than two is invalid: the
panel does not open and the status line shows `Usage: /code-review [<from> <to>]`.
Pressing `r` refreshes the snapshot while preserving the selected revision pair.

Loop intervals accept positive bare seconds (`/loop 300 check status`) or compact
`s`, `m`, `h`, and `d` units (`/loop 3s echo hello`, `/loop 30m check deploy`).
Values are honored exactly; zero and overflow are rejected.

## REPL-only additions

The line REPL also accepts:

| Command | Description |
|---------|-------------|
| `/think <level>` | Set reasoning level (`off`, `minimal`, `low`, `medium`, `high`, `xhigh`, `max`) |
| `!command` | Run a bash command recorded in context |
| `!!command` | Run a bash command excluded from context |

In the TUI, reasoning level and model cycling are controlled by keybindings instead. See [`tui.md`](tui.md).

## TUI-only slash commands

| Command | Description |
|---------|-------------|
| `/theme [name\|next\|prev]` | Show or switch the active theme |
| `/scoped-models` | Enable or disable models for cycling (opens TUI panel) |

## TUI keybindings

The TUI uses the terminal's normal screen with inline redraws, preserving terminal
and tmux scrollback. The transcript is above a composer and status line.

| Key | Action |
|-----|--------|
| `Enter` | Submit the current message |
| `Shift+Enter` or `Ctrl+J` | Insert a newline |
| `Ctrl+D` (on empty input) | Quit |
| `Esc` | Cancel the in-flight run, or clear input when idle |
| `Ctrl+C` | Clear input |
| `Ctrl+U` | Clear the entire composer |
| `Tab` | Accept the selected slash-command completion |
| `Up` / `Down` | Move the cursor, select a completion, or navigate submitted prompt history |
| `Left` / `Right` | Move cursor |
| `Backspace` / `Delete` | Edit text |
| `Home` / `End` | Move to start/end of line |
| `Ctrl+V` / `Alt+V` | Paste from clipboard |
| `Ctrl+X` / `Ctrl+Shift+C` | Copy last assistant message |
| `Ctrl+G` | Open external editor |
| `Ctrl+L` | Open model selector |
| `Ctrl+P` / `Ctrl+Shift+P` | Cycle model forward/backward |
| `Ctrl+T` | Toggle thinking-block visibility |
| `Shift+Tab` | Cycle thinking level |
| `Ctrl+O` | Expand tools panel |

Type `/hotkeys` in either interactive mode to see a shortcut summary. Default bindings can be overridden with custom keybinding JSON files. See [`tui.md`](tui.md).

## Session resume and import

Native sessions are Pi v3 JSONL files. Each line is an object with a `type` field. The active branch is reconstructed from `parentId` chains, so sessions can contain forks and the latest branch is followed on resume.

Session files live under:

```text
<agent-dir>/sessions/--<workspace>--/<timestamp>_<id>.jsonl
```

`<workspace>` is the encoded cwd: the absolute path with leading separators removed and `/`, `\`, and `:` replaced by `-`.

The full-screen TUI selectors opened by `/resume` and `/sessions`, the line REPL bare `/resume` command, and the TUI startup Recent sessions list all draw from the same supported unified catalog. Automatic lists are scoped to the current working directory and display a `[source]` badge next to each row. Supported sources are native Pi (`pi`), OMP (`omp`), Codex (`codex`), Claude (`claude`), Grok/Hyper (`grok/hyper`), and Droid (`droid`).

`--resume` and `/resume` share this catalog. Native selections open the existing file without copying. Foreign selections are converted to Pi v3 once into the effective native session root, retain source lineage, and reuse that converted file on later resumes. In selectors, foreign source files are read-only: they cannot be renamed or deleted. Native Pi sessions and already-imported conversions are regular JSONL files under the session root and can be renamed or deleted from the selector. Only convertible user/assistant text messages are preserved during import; tool calls, reasoning, attachments, and branches are dropped.

`--continue` remains native-only and resumes the newest native Pi session for the directory.

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
