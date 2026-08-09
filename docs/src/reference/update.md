# Update safety

`rpi` can update itself from GitHub releases and reconcile configured rpi packages.
Both paths are designed so a failed update never leaves the installation in an
unusable state.

## Update notifications

When you start an interactive session (TUI or REPL), `rpi` checks GitHub
releases in the background. If a newer release exists, a non-fatal status
message is shown:

```text
Update available: current v0.2.5, latest v0.2.6 — summary — URL (run `rpi update --self`)
```

Source: `crates/pi-cli/src/self_update.rs:187-213`.

Two environment variables control this check:

- `PI_OFFLINE=1|true|yes` disables all updater networking.
- `PI_SKIP_VERSION_CHECK` disables only the interactive startup version check.

## Self-update

```sh
rpi update          # update rpi itself (default when no package flags are given)
rpi update --self   # explicit
rpi update --self --force   # reinstall even when version and checksum match
```

Source: `crates/pi-cli/src/args.rs:270-288`, `crates/pi-cli/src/lib.rs:85-94`.

`rpi update --self` downloads the latest GitHub release for the current platform,
verifies it, smoke-tests it, and activates it atomically. It fails early if
`PI_OFFLINE` is enabled. The updater expects a managed install layout rooted at
`$PI_HOME` (default `~/.rpi` on Unix, `%USERPROFILE%\.rpi` on Windows; the
self-updater otherwise derives the root from the running executable's location)
with an `update-state.json` file.
Source: `crates/pi-cli/src/self_update.rs:211-223`, `crates/pi-cli/src/self_update.rs:480-524`.

### What the self-update does

1. Selects the release. Stable versions query `/releases/latest`. Prerelease
   versions pick the newest published prerelease. Drafts and unpublished
   releases are rejected.
2. Locates the platform archive (`rpi-<version>-<triple>.tar.gz` or `.zip`) and
   the release's `SHA256SUMS` file.
3. Enforces size limits: archives are capped at 1 GiB and `SHA256SUMS` at 1 MiB.
4. Downloads `SHA256SUMS`, looks up the expected digest, and skips the download
   when the installed digest already matches (unless `--force` is used).
5. Downloads the archive, verifies its SHA-256 digest against `SHA256SUMS`, and
   extracts the binary to a staged path.
6. Runs a smoke test: the staged binary must print exactly `rpi <version>` from
   `--version`.
7. Atomically installs the versioned binary and swaps the active symlink, then
   writes `update-state.json` atomically.

Sources: `crates/pi-cli/src/self_update.rs:223-304`,
`crates/pi-cli/src/self_update.rs:705-723`,
`crates/pi-cli/src/self_update.rs:725-770`,
`crates/pi-cli/src/self_update.rs:795-875`,
`crates/pi-cli/src/self_update.rs:874-889`.

### Safety guarantees

- **Checksum verification** — every archive is checked against the release's
  `SHA256SUMS` manifest.
- **Size limits** — archives and extracted binaries are capped at 1 GiB;
  `SHA256SUMS` is capped at 1 MiB.
- **Smoke test** — the downloaded binary must print exactly `rpi <version>`
  from `--version` before activation; exit status alone is not proof of
  identity.
- **Atomic activation** — the active symlink is swapped with `rename(2)` on Unix
  and `MoveFileEx` on Windows, so the active `rpi` path is never missing during
  an update.
- **Rollback** — if smoke testing, activation, or state writing fails, the
  previous active symlink and `update-state.json` are restored.
- **Serialized installs** — a lockfile prevents concurrent installers from
  racing on the same `PI_HOME`.
- **No partial install** — a failed transaction removes staged files and leaves
  the previous binary active.

Source: `crates/pi-cli/src/self_update.rs:250-253`,
`crates/pi-cli/src/self_update.rs:280-284`,
`crates/pi-cli/src/self_update.rs:601-728`,
`crates/pi-cli/src/self_update.rs:705-770`,
`crates/pi-cli/src/self_update.rs:795-875`,
`crates/pi-cli/src/self_update.rs:897-929`,
`crates/pi-cli/src/self_update.rs:1081-1147`.

### Windows deferred activation

On Windows the running executable cannot be replaced while it is executing, so
the self-updater writes a deferred activation script and a
`last-update-result.json` status file. The new binary is moved into place by a
short-lived PowerShell process after the current `rpi` process exits. The
deferred activation then re-verifies that the moved binary prints exactly
`rpi <version>` and restores the previous binary on any mismatch or rollback
failure.
Source: `crates/pi-cli/src/self_update.rs:798-875`,
`crates/pi-cli/src/self_update.rs:987-1025`.

### Update state

After a successful install the updater writes `$PI_HOME/update-state.json`. It
records the installed version, asset name, archive digest, versioned binary
path, and install timestamp. On the next update the digest is used to detect
republished tags that point to a different archive.
Source: `crates/pi-cli/src/self_update.rs:62-73`,
`crates/pi-cli/src/self_update.rs:285-295`.

## Update packages

```sh
rpi update --extensions         # reconcile every configured package (--all is an alias)
rpi update OWNER/REPO           # update one configured git or local package
rpi update local:./my-tools     # update a configured local package
rpi update --self --extensions  # update packages, then update rpi itself
```

Source: `crates/pi-cli/src/args.rs:270-288`,
`crates/pi-cli/src/package_commands.rs:69-94`.

`rpi update --extensions` re-clones or checks out every configured git package
and re-discovers every configured local package. Git packages are checked out
into a content-addressed directory under the agent directory. Pinned git refs
are honored; unpinned sources follow the remote's default branch. Local
packages are validated from their configured paths.
Source: `crates/pi-coding/src/packages.rs:470-708`,
`crates/pi-coding/src/packages.rs:1118-1253`.

Package updates use the same safety patterns as install:

- Operations are serialized with a per-scope lock.
- Git is invoked directly with an argv vector, never through a shell.
- New checkouts are staged next to the existing one and activated with an
  atomic directory swap. If validation fails, the swap is rolled back.
- Settings and package state files are written atomically (temp file + rename)
  and rolled back if either write fails.

Source: `crates/pi-coding/src/packages.rs:656-708`,
`crates/pi-coding/src/packages.rs:1874-1955`,
`crates/pi-coding/src/packages.rs:2033-2133`.

`npm:` package sources are deliberately not supported. They are rejected with a
clear error (`npm package sources are not supported yet; use a local path or git
source`). See [`packages.md`](packages.md) for the supported package sources and
manifest format.
Source: `crates/pi-coding/src/packages.rs:948-954`.

## Update environment variables

| Variable | Default | Purpose |
|----------|---------|---------|
| `PI_HOME` | `~/.rpi` (Unix) / `%USERPROFILE%\.rpi` (Windows) | Install root for the binary and update state |
| `PI_UPDATE_BASE_URL` | `https://api.github.com/repos/0x8f701/rpi/releases` | Release API base (must match the installer scripts) |
| `GITHUB_TOKEN` | (none) | Authenticate GitHub API calls for release metadata |
| `PI_OFFLINE` | (none) | Disables all updater networking |
| `PI_SKIP_VERSION_CHECK` | (none) | Disables only the nonfatal interactive startup version check |

Source: `crates/pi-cli/src/self_update.rs:1-13`,
`crates/pi-cli/src/self_update.rs:95-103`.

## Release policy

- Tags must be semantic versions of the form `vX.Y.Z` (optionally with `+build`
  metadata).
- Prerelease tags (`vX.Y.Z-alpha.N`) are published as prereleases and are not
  made "latest", so the default `/releases/latest` endpoint never points to
  them.
- The release workflow refuses to overwrite an already-published release and
  verifies the asset inventory before publishing.
