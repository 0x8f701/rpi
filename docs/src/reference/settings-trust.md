# Settings, trust, and atomic generations

Settings load in global and project scopes. The project layer is admitted through project trust; locating a project configuration file does not authorize executing its resources.

## Files and effective values

Within each scope, `settings.toml` takes precedence over `settings.json` when present. File extension determines parsing: `.toml` selects TOML; other settings paths use JSON. Writes target the file the loader selects. Unknown fields are retained in typed extra fields rather than silently dropped.

The effective runtime view recursively expands `$NAME` and `${NAME}` in string values. Keys and non-string values are unchanged. Missing environment names remain literal and produce diagnostics. Credential grant definitions and Model Context Protocol credential-grant references stay literal authority identifiers.

Persisted layers retain the original references: editing settings does not write expanded secrets back to disk. Settings writes use a same-directory temporary file and atomic replacement with owner-only permissions.

## Inspect and change

```sh
rpi config list
rpi config get retry.maxRetries
rpi config set retry.maxRetries 3
rpi config reset retry.maxRetries
rpi reload
```

Settings commands accept `--scope global|project` and `--json`. `config list --category CATEGORY` filters the catalog. Typed values are validated; arrays and objects are JSON values.

The terminal `/settings` surface supports inspect/list, search, set, reset, validate, apply, and cancel. Its scope switches are `--global` and `--project`. The Web settings panel uses the same catalog and draft/apply authority rather than writing an independent settings store.

## Trust

`--approve` accepts project-local resources for the current run; `--no-approve` refuses them. Stored project trust and host trust hooks participate in resource admission. These choices are separate from host tool approval mode (`yolo`, `write`, or `ask`).

Do not use a project file to authorize itself. A denial must not be upgraded by a project extension. Host `pre_trust_decision` hooks execute before admitting project extensions; see [hooks](hooks.md).

## Atomic runtime publication

An application generation groups resource, tool, extension, orchestration, runtime, and security state under one nonzero revision and provenance. Construction rejects mismatched revisions or provenance. Reload prepares a new generation from an existing revision instead of independently swapping each subsystem.

A settings file edit and an effective runtime change are not identical events. Inspect the catalog's behavior and the apply result for the affected key. Failed validation is not permission to partially publish a candidate configuration.

## Safety

Secret catalog fields are redacted in views and are not ordinary round-trippable editor values. Never persist a redacted display value over a real credential. Protect global and project configuration files and review resource changes before granting trust.

See [profiles](configuration-profiles.md), [environment variables](environment-variables.md), and [sandbox confinement](sandbox-isolation.md).
