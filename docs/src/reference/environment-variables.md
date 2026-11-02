# Environment variables

Environment configuration is read at defined boundaries; it is not a live substitute for the settings/apply interface.

## Configuration selectors

| Variable | Behavior |
| --- | --- |
| `PI_CODING_AGENT_DIR` | Highest-priority agent configuration base |
| `RPI_HOME` | Configuration base when the preceding override is absent or empty |
| `PI_PROFILE` | Startup profile when `--profile` is absent |
| `PI_AUTH_SCOPE` | Active credential scope; overrides `authScope` |
| `HOME` | Home-directory input for the ordinary user `.rpi` base on Unix |

A named profile appends `profiles/<name>` to the resolved base. The CLI establishes profile selection before dispatch, so settings, auth, sessions, memory, and skills use the same relocated base.

## Operator networking and sharing

| Variable | Behavior |
| --- | --- |
| `RPI_OFFLINE` | Truthy `1`, `true`, or `yes` disables updater networking and gist sharing; web search also observes offline state |
| `RPI_SKIP_VERSION_CHECK` | Suppresses the nonfatal interactive startup update notice only |
| `RPI_UPDATE_BASE_URL` | GitHub-Releases-shaped metadata endpoint for installer/updater operations |
| `PI_SHARE_VIEWER_URL` | Viewer URL or template containing `{url}` for a created gist |
| `GITHUB_TOKEN` | Installer authentication for the fixed GitHub API endpoint; not forwarded to asset hosts |

Installer variables do not redefine runtime configuration precedence. The Unix installer rejects `PI_HOME` and `PI_UPDATE_BASE_URL` rather than silently treating them as current names.

## Settings expansion

String values in the effective settings view expand `$NAME` and `${NAME}` recursively through objects and arrays. Undefined names remain literal and are diagnosed. Keys and non-string values are not expanded. Persisted settings retain the original strings, preventing an ordinary rewrite from storing expanded secrets.

Credential grant definitions and Model Context Protocol credential-grant references remain literal authority identifiers; they are not interpolation fields.

## Credentials and child environments

Provider credential resolution supports provider environment configuration and stored credentials. Use `rpi login`, `rpi setup`, and the active provider configuration rather than inventing a universal API-key environment variable.

A supervised process request can carry an `env` map whose values are strings or null. That request still passes the governed spawn boundary; supplying a map is not permission to bypass sandbox or credential audience restrictions.

## Safety

Do not dump the process environment, credential values, or expanded settings into support reports. Report the variable name, whether it is configured, the selected profile, and the secret-free diagnostic. `--offline` disables nonessential startup networking; it is not a network-denial environment policy.

See [profiles](configuration-profiles.md), [authentication](../user-guide/authentication.md), and [sandbox confinement](sandbox-isolation.md).
