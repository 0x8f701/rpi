# Security

This page describes the trust boundaries and hardening that are implemented in
the current `rpi` source. Every protection below is source-grounded; unsupported
surfaces are called out explicitly rather than with vague limitations.

- [Trust boundary and project-local resources](#trust-boundary-and-project-local-resources)
- [Credential storage and redaction](#credential-storage-and-redaction)
- [Path and symlink containment](#path-and-symlink-containment)
- [Structured stdout and protocol isolation](#structured-stdout-and-protocol-isolation)
- [Extension manifest, environment, and process isolation](#extension-manifest-environment-and-process-isolation)
- [Process ownership, capability gating, and log bounds](#process-ownership-capability-gating-and-log-bounds)
- [Image decode and display limits](#image-decode-and-display-limits)
- [Installer and update checksums / atomicity](#installer-and-update-checksums--atomicity)
- [Reporting issues](#reporting-issues)

## Trust boundary and project-local resources

The runtime keeps global configuration in the agent directory
(`<agent-dir>`, resolved from `$PI_CODING_AGENT_DIR` or the platform default).
Project-local resources live under `$CWD/.pi/` and are only loaded when that
project is trusted. See [`settings-trust.md`](settings-trust.md) for the full
configuration model.

Trust decisions are stored in `$PI_CODING_AGENT_DIR/trust.json`, versioned as
`TRUST_STORE_VERSION = 1`, and keyed by canonical project path. The resolver
walks parents so a decision at `<workspace>` covers `<workspace>/project`.
Source: `crates/pi-coding/src/trust.rs`.

Effective trust is resolved by `resolve_project_trust` in this order:

1. One-run `--approve` / `--no-approve` override.
2. If the project has no `.pi` directory at all, treat it as trusted for that
   run (there is nothing local to load).
3. A persisted decision from `trust.json`.
4. `settings.json#defaultProjectTrust` (`ask`, `always`, `never`).

Headless modes (`--print`, `--mode json`, `--mode rpc`) never prompt. In
headless mode an unset `"ask"` decision is treated as **untrusted**, so pass
`-a` / `--approve` when you need project-local resources:

```sh
rpi --mode json --approve "what does this project do?"
```

Trust-gated project-local resources include skills, prompts, themes,
keybindings, extensions, packages, and `.pi/settings.json`. Global resources
and the built-in model catalog are always loaded. `AGENTS.md` / `CLAUDE.md`
files discovered in ancestors of the cwd are loaded only when the project is
trusted and are included as plain-text instructions, not parsed as
configuration. Sources: `crates/pi-coding/src/trust.rs`,
`crates/pi-coding/src/resources.rs`, `crates/pi-coding/src/resource_manager.rs`.

## Credential storage and redaction

API keys and subscriptions are resolved in the precedence order described in
[`authentication.md`](authentication.md). The final value may come from an
environment variable, `auth.json`, `models.json`, or the `--api-key` flag.

### Storage

`auth.json` stores credentials as a `Credential` enum (`ApiKey` or `OAuth`).
Writes go through `write_credentials_atomic`:

- The parent directory is created and set to `0o700` on Unix.
- A temporary file is created with `0o600` permissions.
- The JSON is serialized, synced, and moved into place with `fs::rename`.
- The final file is set to `0o600` and the parent directory is synced.

Command-valued stored values (`!command`) are rejected during parsing.
`$VAR` / `${VAR}` templates are expanded from the request env map, the
credential's own env map, and then the process environment; empty values are
treated as unset. Expansion errors name the missing variable and source only
and never include the resolved secret. Source: `crates/pi-coding/src/auth.rs`.

### Redaction

- `Credential` and `RequestAuth` `Debug` impls only expose the credential type,
  whether a key is present, and counts of headers/env entries.
- `--api-key` is documented as "never logged". Source: `crates/pi-cli/src/args.rs`.
- The HTTP headers `authorization`, `x-api-key`, `x-goog-api-key`, and
  `cf-aig-authorization` are marked sensitive with `reqwest`'s
  `HeaderValue::set_sensitive(true)`. Source: `crates/pi-ai/src/providers/common.rs`.
- Provider implementations redact those values from error bodies before they
  are returned or logged, replacing secrets with `[REDACTED]`.
- Custom model headers are merged case-insensitively and kept single-valued so
  duplicate `authorization` lines cannot leak a stale value.

Do not commit `auth.json` or `models.json` containing real keys.

## Path and symlink containment

All built-in file tools operate relative to the configured `--cwd`.

- `crates/pi-coding/src/tools/paths.rs::resolve_scoped_path` resolves the path,
  rejects traversal outside the lexical working directory, canonicalizes the
  nearest existing ancestor, and checks that the canonical path still starts
  with the canonical cwd. This prevents symlink escapes.
- `crates/pi-cli/src/file_args.rs::resolve_contained_file` applies the same
  containment to `@file` arguments in the prompt: absolute paths, `..`, root
  components, and prefix components are rejected; the joined path is
  canonicalized and verified to stay inside the cwd. Text `@file` inputs are
  capped at 8 MiB and image `@file` inputs at 20 MiB.
- System-prompt file paths (`--system-prompt`, `--append-system-prompt`) use
  the same symlink-escape check. Source: `crates/pi-cli/src/session_run.rs`.

Extension manifest executable and Bun entry paths are resolved relative to the
manifest directory and must remain inside it. Source:
`crates/pi-coding/src/extensions.rs::resolve_manifest_path`.

Package sources are validated before any git command runs:

- `npm:` sources are rejected at every parse/install/remove entry point with
  the exact error below. Configured `npm:` entries are also skipped during
  discovery so they can never reach installed state.
- Git refs are validated in `validate_git_reference`: leading `-`, `/`,
  trailing `/` or `.`, `..`, `//`, `@{`, backslash, colon, `?`, `*`, `[`, `^`,
  `~`, spaces, control characters, and `.lock` suffixes are all rejected.
- Git hosts and repository paths are normalized and percent-decoded before the
  check. Source: `crates/pi-coding/src/packages.rs`.

The self-updater rejects symlinks for the install root, update state, versioned
binary, and active executable, and verifies that the running binary matches the
managed update state. Source: `crates/pi-cli/src/self_update.rs`.

## Structured stdout and protocol isolation

`rpi` keeps structured output channels separate from interactive terminal
controls.

- `--mode json` emits one JSON line per application event and flushes after each
  record. Source: `crates/pi-cli/src/modes/json.rs`.
- `--mode rpc` reads LF-terminated JSONL commands from stdin and writes
  LF-terminated JSONL responses/events to stdout. Malformed frames produce a
  JSON error response, not raw panic text. Source: `crates/pi-cli/src/modes/rpc.rs`.
- Process extensions speak a strict LF JSONL protocol over stdin/stdout. The
  host rejects CRLF, blank lines, and frames larger than the configured maximum
  (default 1 MiB, minimum 1,024 bytes). Source: `crates/pi-coding/src/extensions.rs`.
- For Bun-hosted extensions, the bundled bridge overrides `process.stdout.write`
  and the global `console` object so extension logs and accidental writes go to
  **stderr**, leaving stdout exclusively for protocol frames. Source:
  `crates/pi-coding/src/bun_extension_host.mjs`.
- Terminal image escape sequences are only emitted by the interactive TUI's
  terminal guard. JSON, RPC, print mode, logs, and ratatui buffers never receive
  raw graphics protocol bytes. Source: `crates/pi-cli/src/terminal_images.rs` and
  `crates/pi-cli/src/tui.rs`.

## Extension manifest, environment, and process isolation

Every extension needs an explicit `pi-extension.json` manifest. See
[`extensions.md`](extensions.md) for the full format.

- The manifest must declare `schemaVersion: 1` and uses `deny_unknown_fields`,
  so extra fields fail closed. The `id` is validated as an identifier up to
  128 bytes. Source: `crates/pi-coding/src/extensions.rs`.
- `runtime` is discriminated: `process` uses `executable` + optional
  `arguments`; `bun` uses `entry` and rejects `arguments`. Bun entries must end
  in `.ts`, `.js`, `.mjs`, or `.cjs`.
- UI capabilities require the `ui` capability; Bun extensions reject
  `message_renderers` and `provider_metadata` because those factories cannot
  cross the process boundary.
- Package extensions are only loaded when the project is trusted. Untrusted
  project manifests are refused. Source:
  `crates/pi-coding/src/extensions.rs::extension_spec_from_package_resource`.

When an extension process is spawned:

- The command starts with `env_clear()`; only the extension's configured
  environment and a small set of host variables are passed through.
- Every process extension receives `PI_EXTENSION_PROTOCOL_VERSION=1`,
  `PI_EXTENSION_ID`, and `PI_EXTENSION_PACKAGE_ID`.
- Bun extensions additionally receive `PI_EXTENSION_ENTRY`,
  `PI_EXTENSION_CAPABILITIES`, `PI_EXTENSION_UI_CAPABILITIES`, and
  `PI_EXTENSION_MAX_FRAME_BYTES`.
- The child is placed in its own process group (`process_group(0)`) and has
  `kill_on_drop(true)`, so it is terminated when the host drops it.
- stdin/stdout/stderr are piped; stdout is protocol-only. Up to 16 KiB of
  stderr is retained for crash diagnostics. Source:
  `crates/pi-coding/src/extensions.rs`.

Bun is resolved from `PATH` or from the absolute executable in
`PI_BUN_EXECUTABLE`; the bridge source is bundled into `rpi` and materialized
independently of the installed package location. Source:
`crates/pi-coding/src/extensions.rs::resolve_bun_executable`.

Extension instances are invalidated when the runtime reloads, the session ends,
or the protocol breaks. The host does not keep orphaned extension children
alive. Source: `crates/pi-coding/src/extensions.rs::spawn_discarded_instances`.

## Process ownership, capability gating, and log bounds

The `ProcessManager` owns long-lived subprocesses and enforces per-owner
boundaries.

- Every spawned process is tagged with a `ProcessOwnerId`. `list`, `describe`,
  `logs`, `write`, `keys`, `resize`, `signal`, `stop`, and `wait` all require the
  caller's `owner_id` to match the session's owner. Source:
  `crates/pi-coding/src/process/manager.rs`.
- Default limits: at most 16 active processes, 1 MiB of retained output per
  process, 30-minute idle timeout with 30-second scans, 1-second termination
  grace, and 256 KiB per log read. Source: `crates/pi-coding/src/process/mod.rs`.
- `validate_spawn_spec` rejects empty argv, NUL bytes in argv or env, non-
  absolute or non-directory cwd, env keys containing `=` or NUL, labels over 64
  bytes, and `output_bytes` above the manager maximum. Source:
  `crates/pi-coding/src/process/manager.rs`.
- Subprocesses are spawned with `env_clear()` plus explicit overrides, their
  own process group, and `kill_on_drop(true)`. On Unix, stdout and stderr are
  merged into a single combined stream so the host sees all output. Source:
  `crates/pi-coding/src/process/backend.rs`.
- `ProcessLog` keeps a bounded ring buffer: when the retained bytes exceed the
  configured capacity, old bytes are dropped and the read response reports how
  much was lost. Source: `crates/pi-coding/src/process/log.rs`.
- The bash tool's `OutputAccumulator` bounds memory with default limits of
  2,000 lines / 50 KiB, keeps a rolling tail of at most 2× the byte limit, and
  streams overflow to a temp file. Source: `crates/pi-coding/src/tools/bash.rs`
  and `crates/pi-coding/src/truncate.rs`.

Extension capabilities are enforced at registration time: the runtime builds
an `ExtensionPermissionSet` from the manifest and rejects any registration that
requests a capability or UI capability not in that set. Source:
`crates/pi-coding/src/extensions.rs::ExtensionPermissionSet::validate_manifest`.

## Image decode and display limits

Image data is bounded before decoding and before being sent to the model or the
terminal.

- `crates/pi-cli/src/image_pipeline.rs` enforces:
  - 20 MiB compressed input (`MAX_IMAGE_BYTES`).
  - 128 MiB decoded allocation budget (`MAX_DECODED_IMAGE_BYTES`).
  - Maximum decoded dimensions capped by that budget at 8 bytes per pixel.
  - Maximum display width/height of 2,000 pixels.
  - Inline base64 cap of ~4.5 MiB.
- Supported formats are PNG, JPEG, GIF, and WebP. The coding-side image tool
  additionally converts BMP to PNG before sending. Source:
  `crates/pi-coding/src/tools/imageresize.rs`.
- Terminal display reuses the same validation. The TUI caches at most 256
  decoded metadata entries, clamps the cell reservation to the viewport, and only
  emits Kitty or iTerm2 graphics protocol bytes. Sixel is detected but not
  emitted because there is no bounded safe encoder yet. Source:
  `crates/pi-cli/src/terminal_images.rs`.

## Installer and update checksums / atomicity

`install.sh`, `install.ps1`, and `rpi update --self` all follow the same
pattern. The full update mechanics are documented in [`update.md`](update.md);
the security-relevant parts are:

- Every release archive is paired with a `SHA256SUMS` file. The installer
  downloads both, enforces a 1 MiB limit on the manifest and a 1 GiB limit on
  the archive, requires exactly one valid 64-character hex digest for the
  platform asset, recomputes the digest locally, and aborts on mismatch.
- A smoke test runs the staged binary with `--version` before activation.
- Unix: the versioned binary is placed with a single `rename(2)`, then the
  active `bin/rpi` symlink is swapped with another `rename(2)`. The active path
  is never missing. The prior symlink target is captured so rollback restores
  exactly what was live. Source: `install.sh` and
  `crates/pi-cli/src/self_update.rs`.
- Windows: `MoveFileEx` with `MOVEFILE_REPLACE_EXISTING` performs an atomic
  same-volume replace. If the running `rpi.exe` is locked, the installer fails
  with a clear message instead of leaving a window where the executable is
  absent. A preemptive backup allows rollback to restore the previous binary.
  Source: `install.ps1` and `crates/pi-cli/src/self_update.rs`.
- `update-state.json` is written atomically (temp file + rename) and is rolled
  back if activation or smoke testing fails. On Windows, activation is deferred
  to a short-lived PowerShell process that runs after the current `rpi` process
  exits. Source: `crates/pi-cli/src/self_update.rs`.
- Concurrent installs are serialized: `install.sh` uses a PID-based lockfile;
  `install.ps1` uses a named mutex; `self_update.rs` acquires an install lock.
- Package updates use the same staging/rollback model: git checkouts are
  prepared next to the live checkout and activated with an atomic directory
  swap, and package state/settings files are written with temp-file + rename
  and rolled back on failure. Source: `crates/pi-coding/src/packages.rs`.

The stable `/releases/latest` endpoint is never used for prereleases: the
installer defaults to `/releases/latest`, and `self_update.rs` rejects draft or
prerelease releases from that endpoint. Prerelease-aware users must explicitly
request a prerelease tag. Source: `install.sh` and
`crates/pi-cli/src/self_update.rs::select_release`.

### npm packages

`npm:` package sources are deliberately not supported. Every entry point that
accepts a package source rejects `npm:` with the exact error:

```text
npm package sources are not supported yet; use a local path or git source
```

There is no partial npm backend, npm package cache, or registry integration.
Supported package sources are `local:` directories below the project and `git:`
repositories. Source: `crates/pi-coding/src/packages.rs::npm_deferred_error`
and [`packages.md`](packages.md).

## Reporting issues

If you find a security issue in `rpi`, please open a private issue or contact
the maintainers before disclosing publicly.
