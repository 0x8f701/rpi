# Installation

## Supported platforms

The release workflow builds five native targets:

- `aarch64-apple-darwin`
- `aarch64-unknown-linux-gnu` (glibc 2.31 baseline, Ubuntu 20.04)
- `x86_64-apple-darwin`
- `x86_64-pc-windows-msvc`
- `x86_64-unknown-linux-gnu` (glibc 2.31 baseline)

Binaries are published as GitHub Release assets named `pi-rs-<version>-<target>.tar.gz`
(or `.zip` on Windows) plus a `SHA256SUMS` manifest. The installers download,
checksum, and atomically activate the matching artifact.

Or use the built-in updater after installation:

```sh
pi update --self
```

## Supported install paths

- **One-line installer** — `install.sh` (macOS / Linux) or `install.ps1` (Windows).
- **GitHub Release asset** — download the matching `.tar.gz` / `.zip` and `SHA256SUMS`, then verify and extract manually.
- **Manual build** — build from source with `cargo`.
- **Self-update** — `pi update --self` after the binary is installed.

See [`docs/update.md`](update.md) for release-check behavior and in-place update safety.

## One-line installer

macOS / Linux:

```sh
curl -fsSL https://raw.githubusercontent.com/0x8f701/pi-rs/main/install.sh | sh
```

Windows (PowerShell):

```powershell
irm https://raw.githubusercontent.com/0x8f701/pi-rs/main/install.ps1 | iex
```

By default the installer resolves the **latest** stable release. It ignores
prereleases because GitHub marks them `make_latest=false`.

## Pin a version

```sh
curl -fsSL https://raw.githubusercontent.com/0x8f701/pi-rs/main/install.sh | bash -s -- --version v0.1.0
```

```powershell
powershell -ExecutionPolicy Bypass -File install.ps1 -Version v0.1.0
```

## What the installer does

1. Detects the host OS/architecture.
2. Fetches release metadata from the GitHub API.
3. Downloads the archive and `SHA256SUMS`.
4. Verifies the archive digest.
5. Extracts the `pi` / `pi.exe` binary.
6. Smoke-tests the binary with `--version`.
7. Writes it to a content-addressed path under `PI_HOME/downloads` and swaps
   the active symlink at `PI_HOME/bin/pi` (or `pi.exe` on Windows) atomically.
8. Records the installed identity in `PI_HOME/update-state.json`.

If any step fails, the installer rolls back the active symlink and leaves the
previous install untouched.

## Installer environment variables

| Variable | Default | Purpose |
|----------|---------|---------|
| `PI_HOME` | `<pi-home>` (platform default unless `PI_HOME` is set) | Install root for the binary and update state |
| `PI_UPDATE_BASE_URL` | `https://api.github.com/repos/0x8f701/pi-rs/releases` | Release API base |
| `GITHUB_TOKEN` | (none) | Authenticates the GitHub API call to avoid unauthenticated rate limits |

The token is sent **only** to the GitHub API endpoint, never to release-asset hosts.

## Manual build from source

Requires Rust **1.88** or later.

```sh
git clone https://github.com/0x8f701/pi-rs.git
cd pi-rs
cargo build --profile release-dist --locked
./target/release-dist/pi --version
```

## Verifying a downloaded release

```sh
curl -fsSL -O https://github.com/0x8f701/pi-rs/releases/download/v0.1.0/pi-rs-0.1.0-x86_64-unknown-linux-gnu.tar.gz
curl -fsSL -O https://github.com/0x8f701/pi-rs/releases/download/v0.1.0/SHA256SUMS
sha256sum -c SHA256SUMS
```

## Directory layout

After installation:

```text
$PI_HOME/
├── bin/
│   └── pi -> ../downloads/pi-rs-<version>-<os>-<arch>-sha256-<digest>
├── downloads/
│   └── pi-rs-<version>-<os>-<arch>-sha256-<digest>
└── update-state.json
```

Runtime configuration and sessions are stored separately under `<agent-dir>/`
(the upstream pi layout). The binary location and the agent directory are
independent, so you can point `PI_CODING_AGENT_DIR` at a different config tree.
