# Configuration profiles, TOML settings, and scoped auth

This page covers the configuration surfaces beyond plain `settings.json`:
config profiles, TOML settings files, environment expansion inside settings,
scoped credentials, and opt-in stateful Responses chaining.

## Config profiles (`--profile`)

`--profile` is a **global** flag that relocates the user base directory —
agent dir, sessions, settings, auth, memory, skills — to
`<base>/profiles/<name>`. `default` keeps the default base; `PI_PROFILE` is
honored when the flag is absent (`crates/pi-cli/src/args.rs:87-92`).

```sh
rpi --profile work sessions
rpi sessions --profile work        # global flag: accepted after subcommands
PI_PROFILE=work rpi                # environment equivalent
```

- Name rules: 1–64 ASCII letters, digits, `-`, or `_` (`default` is valid and
  selects the default profile); whitespace is trimmed first. Anything else
  (slashes, dots, spaces, non-ASCII) fails with an actionable error
  (`validate_profile_name` in `args.rs:231-249`).
- Each profile has its own `settings.json`, `auth.json`, `models.json`,
  `trust.json`, `skills/`, `sessions/`, and resource trees — profile
  isolation is by directory, so credentials, trust decisions, and memory
  never leak between profiles.

## TOML settings

Each scope's settings file is chosen **deterministically by name**, never by
content sniffing: a `settings.toml` sitting next to the canonical
`settings.json` wins when present; otherwise the canonical JSON file is used.
A `.toml` extension selects TOML parsing/serialization; any other extension
(or none) selects JSON (`crates/pi-coding/src/settings.rs:2277-2299`).

- TOML files round-trip through the typed `Settings` struct; unknown fields
  are retained in the `extra` maps, so mixed-format documents survive a
  settings write.
- Settings writes target whichever file the loader would read (JSON→JSON,
  TOML→TOML).
- TOML has no null: JSON nulls inside retained `extra` maps are dropped on
  serialization; datetimes become their string form.

```toml
# settings.toml next to settings.json wins
defaultProvider = "openai"
defaultModel = "openai/gpt-5"
approvalMode = "write"

[orchestration]
tasks = true
maxConcurrency = 8
isolation = "worktree"

[memory]
backend = "local"

[mcp_servers.my-tools]
transport = "stdio"
command = "npx"
```

## Environment expansion in settings

`$NAME` and `${NAME}` references in **every string value** of the settings
document are replaced with the matching process-environment value,
recursively through nested objects and arrays (including retained unknown
fields). Keys and non-string values are never expanded
(`expand_env_in_value_with` in `settings.rs:2375-2439`).

Important properties:

- Expansion applies **only when projecting the effective runtime view**
  (`SettingsManager::settings`, consumed by sessions). The persisted layers
  keep the raw references, so writing settings never persists expanded
  secrets (e.g. `mcpServers[].env` tokens).
- Unset names are left verbatim (fail-open) and reported once per name
  through the settings diagnostic channel; there is no default-value syntax.
- Expansion is recursive over the runtime view only; the persistence view
  keeps literals.

```json
{
  "mcpServers": [
    { "name": "local", "command": "$MCP_CMD", "args": ["-y", "${MCP_PKG}"] }
  ],
  "sandbox": { "allowedPaths": ["$PROJECT_ROOT", "${CACHE_DIR}/pi"] }
}
```

## Scoped credentials (`--scope`)

`rpi login` and `rpi logout` accept a scope label:

```sh
rpi login anthropic --scope work
rpi login anthropic --scope personal
rpi logout xai --scope personal
```

The **active scope** is `PI_AUTH_SCOPE` (environment) or
`settings.authScope` (`settings.rs:911-915`). A scoped credential is selected
over the unscoped default when the active scope matches its label, which
lets one machine hold multiple credentials per provider (work vs. personal)
and pick between them per project or per shell.

## Opt-in stateful Responses chaining

`responsesStatefulChain` (`settings.rs`; default `false`) opts into
stateful turn chaining for OpenAI Responses API models
(`crates/pi-ai/src/providers/responses.rs:30-43`): the provider keeps the
previous response id per session and sends it as `previous_response_id` on
the next turn, sending only the new input items instead of the full
conversation history.

- While enabled, every response is stored server-side (`store: true`) —
  both the seed response and each chained response — because the provider
  must resolve `previous_response_id` from stored responses. Stateless mode
  (`store: false`) never chains.
- The chain is per session in process memory (a session resumed from disk
  starts fresh — the stored id would not match a foreign transcript). It
  breaks (falls back to full history) after consecutive failures or when the
  session transcript is replaced wholesale (compaction resets it,
  `reset_responses_chain`).
- Ignored by every other provider.

## Invariants

- Profile names are validated before any use; `default` and empty select the
  default base, and profile isolation is by directory (no cross-profile
  credential/trust leakage).
- Settings file format is name-determined, never sniffed; a `.toml`
  extension always parses as TOML.
- Environment expansion never mutates the persisted document — expanded
  secrets cannot be written back.
- Scoped credentials only ever *add* a labeled choice; the unscoped default
  remains for providers/sessions without a matching scope.

## Related documentation

- [`settings-trust.md`](settings-trust.md) — `settings.json` fields and
  trust
- [`authentication.md`](../user-guide/authentication.md) — credential
  precedence
- [`environment-variables.md`](environment-variables.md) — `PI_PROFILE`,
  `PI_AUTH_SCOPE`, and all env vars
