# Configuration profiles, TOML settings, and scoped auth

Profiles separate configuration bases. Credential scopes select credentials within a base. Global/project settings scopes select the settings layer to inspect or edit. These three mechanisms are not interchangeable.

## Profile selection

`--profile NAME` takes precedence over `PI_PROFILE` and is fixed before command dispatch. `default` uses the ordinary base. Other names relocate agent-directory-derived settings, authentication, sessions, memory, and skills to `<base>/profiles/<name>`.

The base resolves in this order: `PI_CODING_AGENT_DIR`, `RPI_HOME`, then the user's `.rpi` directory. Empty overrides do not select an empty path. Profile names are 1–64 ASCII letters, digits, hyphens, or underscores after trimming.

```sh
rpi --profile work setup
rpi --profile work models
rpi --profile default sessions
```

Project-local `.rpi` resources remain project resources subject to project trust. A profile is configuration separation, not an operating-system sandbox.

## Settings format

Within each scope, `settings.toml` wins over `settings.json`. Parsing and serialization follow the selected filename, not content sniffing. Unknown fields survive typed round-tripping. Settings operations write the file the loader reads.

Runtime environment expansion applies recursively to string values, preserving literal references in persisted layers. Missing names remain literal and generate diagnostics. Credential authority identifiers are not expanded.

Use `rpi config list --json` to inspect actual supported keys, types, source, and behavior instead of copying an obsolete full settings example.

## Credential scope

`rpi login PROVIDER --scope work` stores a scoped credential. `PI_AUTH_SCOPE` overrides the `authScope` setting. The selected scope wins when present; an unscoped credential is used otherwise if available. A scope selection failure reports available labels, not credentials.

Changing a profile changes the credential file being read. Changing credential scope does not move sessions or resources. See [authentication](../user-guide/authentication.md).

## Safety

Inspect the active paths with `rpi setup` before editing or deleting configuration. Do not copy credential files into a repository or assume a new profile grants project trust. Settings edits use the shared validation and atomic apply path described in [settings and trust](settings-trust.md).
