# Quick start

## First-run workflow

1. **Install** `pi`:
   ```sh
   curl -fsSL https://raw.githubusercontent.com/0x8f701/pi-rs/main/install.sh | sh
   ```
   See [`install.md`](install.md) for Windows, pinned versions, manual builds, and verification.

2. **Verify** the binary:
   ```sh
   pi --version
   ```

3. **Configure credentials** with an environment variable:
   ```sh
   export OPENAI_API_KEY="<your-openai-key>"
   export ANTHROPIC_API_KEY="<your-anthropic-key>"
   export GEMINI_API_KEY="<your-gemini-key>"
   ```
   For `auth.json`, `models.json`, `pi login`, and precedence rules, see [`authentication.md`](authentication.md).

4. **Run a non-interactive task** to confirm the end-to-end path:
   ```sh
   pi --print -m anthropic/claude-sonnet-4-5 "List the Rust files in this directory"
   ```

5. **Start an interactive session**:
   ```sh
   pi -m anthropic/claude-sonnet-4-5
   ```
   The CLI picks the full-screen TUI when stdout is a terminal and raw mode works; otherwise it falls back to the line REPL. See [`cli-modes.md`](cli-modes.md) for every flag, subcommand, and slash command.

## Non-interactive print mode

```sh
pi --print "Explain this crate" -m anthropic/claude-sonnet-4-5

# Or stream events as JSON lines for tooling
pi --mode json -m openai/gpt-5.5 "List all unfinished task markers in src/"
```

Print mode is also implied when you pass a positional prompt:

```sh
pi -m openai/gpt-5.5 "List all unfinished task markers in src/"
```

If the joined prompt is empty (for example `pi ""`), the CLI enters the
interactive session instead of failing. See [`cli-modes.md`](cli-modes.md).

## Interactive session

```sh
pi -m anthropic/claude-sonnet-4-5
```

- If `stdout` is a terminal and the TUI can enter raw mode, you get the
  full-screen TUI.
- Otherwise you get the line REPL (`> ` prompt).

In both modes you can type a message or use slash commands. Type `/help` for
available commands. Model and thinking-level switches work as slash commands in
the line REPL (in the TUI use keybindings such as `Ctrl+L` and `Ctrl+T`).

## Switch models and reasoning level

In the line REPL:

```
/model openai/gpt-5.5
/think high
```

From the shell you can set the initial model and thinking level:

```sh
pi -m openai/gpt-5.5 --think high --print "Refactor this function"
```

See [`models.md`](models.md) for model-spec syntax and custom providers.

## Manage sessions

```sh
# List sessions for the current directory
pi sessions

# Resume the newest session for this directory
pi --continue

# Resume a native or foreign session by path, exact id, or prefix
pi --resume rollout-abc123.jsonl
pi --resume abc123
```

Sessions are stored as append-only JSONL files under `<agent-dir>/sessions/`.
See [`cli-modes.md`](cli-modes.md#session-resume-and-import) for path encoding and import details.

## Discover models

```sh
pi models
pi models claude
pi models openai
```

The filter is case-sensitive and matches against provider name or model id.

## Log in

```sh
pi login
pi login anthropic
```

`login` stores credentials in `auth.json`. `logout` removes them. For
non-interactive configuration, use `auth.json` or environment variables (see
[`authentication.md`](authentication.md)).

## Export or share a session

```sh
# Export a session file to a self-contained HTML file
pi export <agent-dir>/sessions/--<workspace>--/timestamp_id.jsonl

# Export the current branch as JSONL for later resume
pi export session.jsonl --jsonl --output backup.jsonl
```

In the TUI or REPL, `/share` creates a private GitHub gist via the `gh` CLI.
See [`export-share.md`](export-share.md) for formats and sharing.

## Use a local model

```sh
pi llama configure http://localhost:8080
pi -m llama/llama-3.1-8b --print "Hello"
```

See [`local-llama.md`](local-llama.md) for router setup and GGUF downloads.

## Manage packages

```sh
pi install git:owner/pi-my-tools
pi list
pi update --extensions
```

Project packages need a trusted project. See [`settings-trust.md`](settings-trust.md)
and [`packages.md`](packages.md).

## Configure loaded resources

```sh
# Open an interactive selector for enabled extensions/skills/prompts/themes
pi config

# Scope the selector to project-local settings
pi config --local
```

Project scope is refused unless the project is trusted. See [`packages.md`](packages.md)
for the package/resource model.

## Next steps

- Read [`cli-modes.md`](cli-modes.md) for the full command surface.
- Read [`settings-trust.md`](settings-trust.md) to configure defaults in
  `settings.json`.
- Read [`models.md`](models.md) to add custom providers or local models.
