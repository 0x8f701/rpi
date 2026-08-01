# Settings, configuration, and trust

## Configuration directory

The runtime resolves the agent configuration directory (`<agent-dir>`) in this
order:

1. `PI_CODING_AGENT_DIR` environment variable.
2. The platform home directory (`HOME` on Unix, `USERPROFILE` on Windows) with
   `/.pi/agent` appended.

Source: `crates/pi-coding/src/resources.rs:26-41`.

Within `<agent-dir>` the CLI reads:

- `settings.json` — global startup defaults and operational settings.
- `models.json` — custom providers and model overrides.
- `auth.json` — stored provider credentials.
- `trust.json` — persisted project trust decisions.
- `sessions/` — native Pi v3 JSONL session files.
- `skills/`, `prompts/`, `themes/` — global resources.

Project-local resources are loaded from `<workspace>/.pi/` only when that project
is trusted. Source: `crates/pi-coding/src/resource_manager.rs:49-70`.

## Project trust

By default, `rpi` asks before loading project-local `.pi` resources (skills,
prompts, themes, keybindings, extensions, agents, and package settings). Trust
decisions are stored in `<agent-dir>/trust.json`, versioned as
`TRUST_STORE_VERSION = 1`, and scoped to the canonical project path. The
resolver walks parent directories, so a decision at `<workspace>` covers
`<workspace>/sub-project`.

Source: `crates/pi-coding/src/trust.rs:67-140` and
`crates/pi-coding/src/trust.rs:180-216`.

Default behavior is controlled by `settings.json`:

```json
{
  "defaultProjectTrust": "ask"
}
```

Allowed values:

- `"ask"` — prompt for trust in interactive modes; treat as untrusted in
  headless modes (default).
- `"always"` — trust every project with a `.pi` directory without asking.
- `"never"` — never trust project-local resources.

One-run overrides:

```sh
rpi -a "hello"           # --approve: trust this project's .pi for this run
rpi --no-approve "hello" # refuse project .pi for this run
```

Headless modes (`--print`, `--mode json`, `--mode rpc`) never prompt. In
headless mode an unset or `"ask"` decision is treated as untrusted, so use
`--approve` when you need project resources. If the project has no `.pi`
directory, it is implicitly trusted.

Source: `crates/pi-coding/src/trust.rs:37-43` and
`crates/pi-cli/src/args.rs:168-174`.

## `settings.json`

Global settings live at `<agent-dir>/settings.json`. Project settings live at
`<workspace>/.pi/settings.json` and are merged on top of global settings when
trusted. Unknown fields are retained across merges so other product modules can
use them.

Source: `crates/pi-coding/src/settings.rs:666-669` and
`crates/pi-coding/src/settings.rs:729-843`.

```json
{
  "defaultProvider": "openai",
  "defaultModel": "openai/gpt-5",
  "defaultThinkingLevel": "medium",
  "defaultProjectTrust": "ask",
  "theme": "dark",
  "compaction": {
    "enabled": true,
    "reserveTokens": 8192,
    "keepRecentTokens": 4096
  },
  "terminal": {
    "showImages": true,
    "imageWidthCells": 80,
    "clearOnShrink": false,
    "showTerminalProgress": true
  },
  "images": {
    "autoResize": true,
    "blockImages": false
  },
  "retry": {
    "enabled": true,
    "maxRetries": 3,
    "baseDelayMs": 2000,
    "provider": {
      "timeoutMs": 60000,
      "maxRetries": 3,
      "maxRetryDelayMs": 60000
    }
  },
  "transport": "auto",
  "timeoutMs": 60000,
  "maxRetryDelayMs": 60000,
  "temperature": 0.7,
  "maxTokens": 4096,
  "cacheRetention": "short",
  "thinkingBudgets": {
    "minimal": 1024,
    "low": 2048,
    "medium": 4096,
    "high": 8192
  },
  "scopedModels": ["openai/*", "anthropic/*"],
  "branchSummary": {
    "reserveTokens": 16384,
    "skipPrompt": false
  },
  "keybindings": {
    "toggleTheme": "ctrl+t"
  },
  "quietStartup": false,
  "showThinking": true,
  "exposeSessionEnvironment": true,
  "doubleEscapeAction": "tree",
  "orchestration": {
    "process": false,
    "tasks": false,
    "todo": false,
    "maxConcurrency": 8,
    "maxRecursionDepth": 8,
    "mailboxCapacity": 1000,
    "maxToolsPerAgent": 16
  },
  "packages": [
    "git:github.com/owner/pi-my-tools",
    { "source": "./my-pi-tools", "extensions": ["*"] }
  ],
  "extensions": ["ext-id"],
  "skills": ["rust-review"],
  "prompts": ["custom-prompt"],
  "themes": ["solarized"]
}
```

### Known fields

| Field | Purpose |
|-------|---------|
| `defaultProvider` | Default provider id used when no model flag is given. |
| `defaultModel` | Default model id used when no `--model` is given. |
| `defaultThinkingLevel` | Default reasoning level (`off`, `minimal`, `low`, `medium`, `high`, `xhigh`, `max`). |
| `defaultProjectTrust` | `"ask"`, `"always"`, or `"never"`. |
| `steeringMode` | Steering queue mode (`all` or `one-at-a-time`). |
| `followUpMode` | Follow-up queue mode (`all` or `one-at-a-time`). |
| `theme` | Initial TUI theme name. |
| `compaction` | Context compaction limits (`enabled`, `reserveTokens`, `keepRecentTokens`). |
| `terminal` | Legacy TUI rendering options (`showImages`, `imageWidthCells`, `clearOnShrink`, `showTerminalProgress`). |
| `images` | Image display options (`autoResize`, `blockImages`). |
| `retry` | Retry policy (`enabled`, `maxRetries`, `baseDelayMs`, plus optional `provider` overrides). |
| `autoRetry`, `maxRetries`, `baseDelayMs` | Top-level legacy aliases for the same retry fields. |
| `transport` | Stream transport (`auto`, `sse`, `web-socket`, `web-socket-cached`). |
| `timeoutMs` | HTTP/stream timeout. |
| `httpIdleTimeoutMs` | Idle HTTP connection timeout. |
| `websocketConnectTimeoutMs` | WebSocket connect timeout. |
| `maxRetryDelayMs` | Maximum delay between retries. |
| `temperature` | Sampling temperature, finite and `0..=2`. |
| `maxTokens` | Maximum tokens per model request. |
| `cacheRetention` | Cache retention (`none`, `short`, `long`). |
| `thinkingBudgets` | Per-level token budgets (`minimal`, `low`, `medium`, `high`). |
| `scopedModels` / `enabledModels` | Alias for the same model-pattern allowlist. |
| `branchSummary` | Branch-summary token reserve and `skipPrompt`. |
| `keybindings` | Action-to-chord map (value is a string or array of strings). |
| `quietStartup` | Suppress non-essential startup messages. |
| `showThinking` / `hideThinkingBlock` | Control whether thinking blocks are shown. |
| `showImages`, `imageWidthCells`, `autoResizeImages` | Top-level legacy aliases for terminal/image options. |
| `exposeSessionEnvironment` | Forward session environment to tools. |
| `doubleEscapeAction` | TUI action on double-escape (`fork`, `tree`, `none`). |
| `orchestration` | Orchestration tool gates and concurrency limits. |
| `selector` | Optional model-selector thresholds (advanced). |
| `packages` | Local/git package sources to install/load. |
| `extensions`, `skills`, `prompts`, `themes` | Resource names to load from configured packages and discovered paths. |

Source: `crates/pi-coding/src/settings.rs:335-408`.

### Supported operational settings coverage

The following keys are the supported operational settings coverage used by the
runtime. Any other field may exist but is either a startup default, a resource
list, or module-specific configuration.

| Setting | Applied by |
|---------|------------|
| `steeringMode` | `steering_mode` |
| `followUpMode` | `follow_up_mode` |
| `retry` | `retry_settings` / `apply_session_options` |
| `compaction` | `apply_session_options` |
| `transport` | `apply_session_options` |
| `timeoutMs` | `apply_session_options` |
| `maxRetryDelayMs` | `apply_session_options` |
| `temperature` | `apply_session_options` |
| `maxTokens` | `apply_session_options` |
| `cacheRetention` | `apply_session_options` |
| `thinkingBudgets` | `apply_session_options` |
| `scopedModels` | `scoped_model_patterns` |
| `enabledModels` | `scoped_model_patterns` (legacy alias) |
| `terminal` | `tui_runtime` |
| `images` | `tui_runtime` |
| `theme` | `tui_runtime` / resource validation |
| `keybindings` | `tui_runtime` |
| `branchSummary` | `branch_summary_settings` |
| `quietStartup` | `tui_runtime` |
| `hideThinkingBlock` | `tui_runtime` |
| `showThinking` | `tui_runtime` |
| `exposeSessionEnvironment` | `expose_session_environment` |
| `doubleEscapeAction` | `tui_runtime` |
| `orchestration` | orchestration tool gates |

Source: `crates/pi-coding/src/settings.rs:302-330`.

## Model and thinking-level precedence

When the CLI starts without an explicit model:

1. `--model` / `-m` flag.
2. Resumed session's saved model (when `--resume` or `--continue` is used).
3. `settings.json` `defaultModel`.
4. First authenticated model in the catalog.

Thinking level precedence:

1. `--think` flag.
2. Thinking level parsed from a `:level` model suffix (`anthropic/claude-sonnet-4-5:high`).
3. Resumed session's saved thinking level.
4. `settings.json` `defaultThinkingLevel`.
5. `medium`.

The final level is clamped to the resolved model's supported levels.

## Live reload semantics

Configuration and resources are loaded into an atomic snapshot.

- `rpi reload` validates the current settings/resource graph and prints a
  structured snapshot, including `generation`, `trust`, discovered resources, and
  diagnostics. It always runs headlessly and emits JSON.
- `SettingsManager::reload()` re-reads global settings and, when the project is
  trusted, project settings, then recomputes the effective snapshot.
- `ResourceManager::stage_reload()` builds a new candidate snapshot without
  replacing the active one. `commit_reload()` swaps the active snapshot only
  after validation succeeds; any error leaves the previous generation in place.
- Interactive TUI `/reload` and REPL `reload` call the same stage-and-commit
  path, so malformed files never silently replace a working configuration.
- Settings writes performed through `update_global` or `update_project` are
  atomic (temp file + `fs::rename` + directory sync). Session-only overrides
  via `apply_overrides` are never persisted.

Source: `crates/pi-cli/src/commands.rs:145-180`,
`crates/pi-coding/src/settings.rs:729-843`, and
`crates/pi-coding/src/resource_manager.rs:139-253`.

## Trust boundaries

- No project-local configuration is loaded unless the project is trusted. Use
  `-a`/`--approve` or set `defaultProjectTrust` explicitly.
- Bash tool commands run in the configured working directory (`--cwd`).
- API keys are never printed in error messages or logs.
- `models.json` and `auth.json` values may contain `$VAR` / `${VAR}` templates,
  which are expanded from the current process environment or an explicit `env`
  map. Command-valued values (`!command`) are rejected.

## Custom config path for tests or isolation

```sh
export PI_CODING_AGENT_DIR="<workspace>/.my-pi-config"
mkdir -p "$PI_CODING_AGENT_DIR"
rpi --print "hello"
```
