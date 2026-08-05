# Quick start

## First-run workflow

1. **Install** `rpi`:
   ```sh
   curl -fsSL https://raw.githubusercontent.com/0x8f701/rpi/master/install.sh | sh
   ```
   See [`install.md`](install.md) for Windows, pinned binary releases, manual
   archive verification, and rollback behavior.

2. **Verify** the binary:
   ```sh
   rpi --version
   ```

3. **Configure one provider credential**. For example, either log in or set the
   environment variable used by the model in the next step:
   ```sh
   rpi login anthropic
   # Or, instead of interactive login:
   export ANTHROPIC_API_KEY="<your-anthropic-key>"
   ```
   Do not export placeholder values. For other providers, `auth.json`,
   `models.json`, and precedence rules, see [`authentication.md`](authentication.md).

4. **Run a non-interactive task** to confirm the end-to-end path:
   ```sh
   rpi --print -m anthropic/claude-sonnet-4-5 "List the Rust files in this directory"
   ```

5. **Start an interactive session**:
   ```sh
   rpi -m anthropic/claude-sonnet-4-5
   ```
   The CLI opens the normal-screen inline TUI when both stdin and stdout are
   terminals; otherwise it uses the line REPL. See [`cli-modes.md`](cli-modes.md)
   for every flag, subcommand, and slash command.

## Non-interactive print mode

```sh
rpi --print "Explain this crate" -m anthropic/claude-sonnet-4-5

# Or stream events as JSON lines for tooling
rpi --mode json -m openai/gpt-5.5 "List all unfinished task markers in src/"
```

Positional prompts initialize the interactive session on a terminal. They select
print mode only when stdin or stdout is not a terminal and `--listen` is absent;
with `--listen`, a non-terminal run stays on the live line REPL path. Use
`--print` when a script must be non-interactive:

```sh
rpi -m openai/gpt-5.5 "Review this repository"
```

## Interactive session

```sh
rpi -m anthropic/claude-sonnet-4-5
```

- If both `stdin` and `stdout` are terminals, you get the normal-screen inline TUI.
- Otherwise you get the line REPL (`> ` prompt).

In both modes you can type a message or use slash commands. Type `/help` for the
14 primary commands, including the TUI-only `/code-review` and `/btw` overlays.
Model and thinking-level switches work as slash commands in the line REPL; the
TUI uses `/model` plus keybindings such as `Ctrl+L` and `Ctrl+T`.

## Switch models and reasoning level

In the line REPL:

```
/model openai/gpt-5.5
/think high
```

From the shell you can set the initial model and thinking level:

```sh
rpi -m openai/gpt-5.5 --think high --print "Refactor this function"
```

See [`models.md`](models.md) for model-spec syntax and custom providers.

## Manage sessions

```sh
# List native Pi sessions for the current directory
rpi sessions

# Resume the newest native Pi session for this directory
rpi --continue

# Resume a native or foreign session by path, exact id, or prefix
rpi --resume rollout-abc123.jsonl
rpi --resume abc123
```

In the interactive TUI and line REPL, `/resume` and `/sessions` open a
current-cwd-scoped unified catalog with `[source]` badges for native Pi, OMP,
Codex, Claude, Grok/Hyper, and Droid sessions. Selecting a foreign session
imports it once into the effective native session root; later resumes reuse
the converted file. Foreign source files cannot be renamed or deleted from the
selector, but native and imported JSONL files can be. `--continue` stays
native-only and resumes the newest native Pi session for the directory.

Sessions are stored as append-only JSONL files under `<agent-dir>/sessions/`.
See [`cli-modes.md`](cli-modes.md#session-resume-and-import) for path encoding,
import details, and selector management rules.

## Discover models

```sh
rpi models
rpi models claude
rpi models openai
```

The filter is case-sensitive and matches against provider name or model id.

## Log in

```sh
rpi login
rpi login anthropic
```

`login` stores credentials in `auth.json`. `logout` removes them. For
non-interactive configuration, use `auth.json` or environment variables (see
[`authentication.md`](authentication.md)).

## Export or share a session

```sh
# Export a session file to a self-contained HTML file
rpi export <agent-dir>/sessions/--<workspace>--/timestamp_id.jsonl

# Export the current branch as JSONL for later resume
rpi export session.jsonl --jsonl --output backup.jsonl
```

In the TUI or REPL, `/share` creates a secret GitHub gist via the `gh` CLI.
See [`export-share.md`](export-share.md) for formats and sharing.

## Use a local model

```sh
rpi llama configure http://127.0.0.1:8080
rpi -m llama.cpp/<model-id> --print "Hello"
```

See [`local-llama.md`](local-llama.md) for router setup and GGUF downloads.

## Manage packages

```sh
rpi install git:github.com/owner/pi-my-tools
rpi list
rpi update --extensions
```

Project packages need a trusted project. See [`settings-trust.md`](settings-trust.md)
and [`packages.md`](packages.md).

## Configure loaded resources

```sh
# Open an interactive selector for enabled extensions/skills/prompts/themes
rpi config

# Scope the selector to project-local settings
rpi config --local
```

Project scope is refused unless the project is trusted. See [`packages.md`](packages.md)
for the package/resource model.

## Next steps

- Read [`cli-modes.md`](cli-modes.md) for the full command surface.
- Read [`settings-trust.md`](settings-trust.md) to configure defaults in
  `settings.json`.
- Read [`models.md`](models.md) to add custom providers or local models.
