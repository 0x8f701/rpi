# CLI modes and commands

## Select an interface

| Invocation | Interface |
| --- | --- |
| `rpi` | Interactive terminal interface when input and output are terminals |
| `rpi --print "Explain this project"` | Noninteractive text output |
| `rpi --mode json "Explain this project"` | JSON event stream |
| `rpi rpc` or `rpi --mode rpc` | JSONL command/response control over standard input/output |
| `rpi --listen 127.0.0.1:8543` | Standalone HTTPS/WebSocket service, independent of standard input |
| `rpi agent stdio` | Agent Client Protocol with Content-Length framing |

Without terminal input or output, the ordinary run path selects print mode. Positional prompts initialize an interactive session; in print and structured modes they are separate turns. JSON event output is not the JSONL RPC command protocol.

## Run options

| Option | Meaning |
| --- | --- |
| `-m`, `--model SPEC` | Provider/model or bare model identifier |
| `--provider PROVIDER` | Provider override; requires `--model` |
| `--models PATTERNS` | Comma-separated interactive model-cycle scope |
| `--thinking LEVEL` | `off`, `minimal`, `low`, `medium`, `high`, `xhigh`, `max` |
| `-C`, `--cwd DIR` | Existing working directory; startup does not create it |
| `--add-dir DIR` | Additional scoped search and file-attachment root; repeatable |
| `-n`, `--name NAME` | Session display name |
| `--system-prompt TEXT_OR_PATH` | Replace the system prompt |
| `--append-system-prompt TEXT_OR_PATH` | Append system instructions; repeatable |
| `--api-key KEY` | Per-run provider credential override; avoid exposing secrets in shell history |
| `--offline` | Disable nonessential startup networking, not all network tool or model access |
| `--verbose` | Startup diagnostics |

## Sessions

- `--continue` (`-c`): newest session for this directory.
- `--resume PATH_OR_ID` (`-r`): native or foreign session path, exact identifier, or unambiguous prefix.
- `--session PATH_OR_ID`: open a session by path or identifier.
- `--session-id ID`: open an exact project session identifier, creating the session when absent.
- `--fork PATH_OR_ID`: fork a session.
- `--session-dir DIR`: override storage and identifier lookup.
- `--no-session`: disable session-file persistence for the run.
- `--profile NAME`: select a configuration profile; `default` selects the ordinary base.

Resume selectors are mutually constrained; do not combine `--continue`, `--resume`, and `--session`. Use [session recovery](session-recovery.md) for branch and import behavior.

## Tools, resources, and trust

`--tools` supplies a comma-separated allowlist; `--exclude-tools` applies a denylist afterward. `--no-tools` disables all tool contributions. `--no-builtin-tools` removes built-ins while retaining other contributions; the two switches are mutually exclusive.

Explicit resources are repeatable: `--extension`, `--skill`, `--prompt-template`, and `--theme`. Their `--no-extensions`, `--no-skills`, `--no-prompt-templates`, and `--no-themes` switches disable discovery/configuration while retaining explicitly supplied paths. `--no-context-files` disables `AGENTS.md` and `CLAUDE.md` discovery.

`--approve` (`-a`) trusts project-local resources for this run. `--no-approve` refuses them. This is separate from `--approval-mode yolo|write|ask`, which controls host tool approval.

## Subcommands

| Command | Purpose |
| --- | --- |
| `login [PROVIDER]`, `logout [PROVIDER]` | Manage credentials; `--scope LABEL` selects a credential scope |
| `models [FILTER]` | List available models |
| `sessions` | List saved sessions for the directory |
| `import-session SOURCE INPUT [--output PATH]` | Convert Pi, OMP, Codex, Claude, Grok, or Droid sessions |
| `export SESSION_PATH [-o PATH] [--jsonl]` | Self-contained HTML or current-branch JSONL |
| `reload` | Validate and print the active resource snapshot |
| `doctor [--json]` | Environment diagnostics |
| `setup [--json]` | Configuration locations and setup guidance |
| `dashboard [--json]` | Session and usage summary |
| `install SOURCE [--local]` | Install a local or Git package |
| `remove SOURCE [--local]` | Remove a configured package |
| `list` | List configured packages |
| `config` | Resource selection or settings operations |
| `update` | Update the managed installation |
| `llama` | Local model/router management |
| `plugin` | Marketplace plugin management |
| `mcp` | Server configuration and import |
| `agent` | Agent Client Protocol transports |
| `completion bash|zsh|fish` | Shell completion output |

`rpi --export SESSION_PATH --output PATH --jsonl` also supports export without starting a model. Settings operations are `config get KEY`, `config set KEY VALUE`, `config reset KEY`, and `config list`; they support `--scope global|project` and `--json`. Use each subcommand's `--help` for its exact argument contract.

## Interactive commands

Slash commands are entered in the interactive composer, not passed as shell subcommands. Important entry points include `/loop`, `/todo`, `/workflow`, `/rewind`, and `/handoff`. Terminal and Web panels expose the corresponding application features where supported. See [TUI](tui.md), [orchestration](orchestration.md), and [Web](web.md).

The model-facing bash tool allows shell `grep` and `rg`. It still rejects unsupervised background/detached work: long-lived commands must use `background=true` or the supervised process tool.

**Steering queue status: in progress.** Queued steering previews are implemented in Web; the Rust queue and scoped cancellation integration are not documented as a stable command contract yet.
