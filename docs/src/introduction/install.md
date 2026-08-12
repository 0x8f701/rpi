# Installation

## Supported platforms

The release workflow builds five native targets:

- `aarch64-apple-darwin`
- `aarch64-unknown-linux-gnu` (glibc 2.31 baseline, Ubuntu 20.04)
- `x86_64-apple-darwin`
- `x86_64-pc-windows-msvc`
- `x86_64-unknown-linux-gnu` (glibc 2.31 baseline)

Prebuilt binaries are published as GitHub Release assets named
`rpi-<version>-<target>.tar.gz` (or `.zip` on Windows), alongside a
`SHA256SUMS` manifest. Installation does not require Rust or a source checkout.
The installers download, verify, and atomically activate the matching binary.

Or use the built-in updater after installation:

```sh
rpi update --self
```

## Supported install paths

- **Prebuilt binary installer** — `install.sh` (macOS / Linux) or `install.ps1` (Windows); recommended for users.
- **Prebuilt GitHub Release asset** — download the matching `.tar.gz` / `.zip` and `SHA256SUMS`, then verify and extract manually.
- **Self-update** — `rpi update --self` after the binary is installed.
- **Source build** — developer fallback requiring Rust 1.88 or later.

See [`update.md`](../reference/update.md) for release-check behavior and in-place update safety.

## One-line installer

macOS / Linux:

```sh
curl -fsSL https://raw.githubusercontent.com/0x8f701/rpi/master/install.sh | sh
```

Windows (PowerShell):

```powershell
irm https://raw.githubusercontent.com/0x8f701/rpi/master/install.ps1 | iex
```

By default the installer resolves the **latest published stable binary release**.
If the release does not contain the exact platform archive and `SHA256SUMS`, the
installer fails without changing the existing installation.

## Pin a version

Pin both the installer script and the requested release tag:

```sh
curl -fsSL https://raw.githubusercontent.com/0x8f701/rpi/v0.2.10/install.sh | bash -s -- --version v0.2.10
```

On Windows, download the script from the same tag before invoking it:

```powershell
irm https://raw.githubusercontent.com/0x8f701/rpi/v0.2.10/install.ps1 -OutFile install.ps1
powershell -ExecutionPolicy Bypass -File ./install.ps1 -Version v0.2.10
```

## What the installer does

1. Detects the host OS/architecture.
2. Fetches release metadata from the GitHub API.
3. Downloads the archive and `SHA256SUMS`.
4. Verifies the archive digest.
5. Extracts the `rpi` / `rpi.exe` binary.
6. Smoke-tests the binary with `--version`.
7. Writes it to a content-addressed path under `PI_HOME/downloads` and atomically
   swaps `PI_HOME/bin/rpi` to that path on Unix. Windows atomically replaces
   `PI_HOME/bin/rpi.exe` because a running executable cannot be a symlink target.
8. Records the installed identity in `PI_HOME/update-state.json`.
9. On Unix, removes a legacy installer-managed `PI_HOME/bin/pi` symlink only
   when it still points at a previous installer-owned download path.

If any step fails, the installer rolls back the active symlink and leaves the
previous install untouched.

When the install directory is not already on `PATH`, the Unix installer updates
the detected shell profile and the Windows installer updates the user `PATH`.
Open a new terminal before running `rpi` after such a change.

## Installer environment variables

| Variable | Default | Purpose |
|----------|---------|---------|
| `PI_HOME` | `~/.rpi` (Unix) / `%USERPROFILE%\.rpi` (Windows) | Install root for the binary and update state |
| `PI_UPDATE_BASE_URL` | `https://api.github.com/repos/0x8f701/rpi/releases` | Release API base |
| `GITHUB_TOKEN` | (none) | Authenticates the GitHub API call to avoid unauthenticated rate limits |

The token is sent **only** to the fixed GitHub API endpoint
(`https://api.github.com/repos/0x8f701/rpi/releases`), never to release-asset
hosts or a custom `PI_UPDATE_BASE_URL` endpoint. `install.sh`, `install.ps1`,
and `rpi update --self` all apply the same scoping.

## Developer source build

This is not the normal installation path. It requires Rust **1.88** or later.

```sh
git clone https://github.com/0x8f701/rpi.git
cd rpi
cargo install --path crates/pi-cli --locked --bin rpi
rpi --version
```

To build a distribution binary in-tree without installing into Cargo's bin
directory:

```sh
cargo build --package pi-cli --bin rpi --profile release-dist --locked
./target/release-dist/rpi --version
```

The JSONL RPC control plane is the `rpi rpc` subcommand (≡ `--mode rpc`), so no
companion binary is built or installed separately.

## Verifying a downloaded release

After a release is published, replace `<version>` and `<target>` with an actual
tag version and one of the supported target triples:

```sh
version="<version>"
target="x86_64-unknown-linux-gnu"
curl -fsSL -O "https://github.com/0x8f701/rpi/releases/download/v${version}/rpi-${version}-${target}.tar.gz"
curl -fsSL -O "https://github.com/0x8f701/rpi/releases/download/v${version}/SHA256SUMS"
sha256sum -c --ignore-missing SHA256SUMS
```

## Directory layout

After installation:

```text
$PI_HOME/   # default ~/.rpi on Unix, %USERPROFILE%\.rpi on Windows
├── bin/
│   └── rpi -> ../downloads/rpi-<version>-<os>-<arch>-sha256-<digest>
├── downloads/
│   └── rpi-<version>-<os>-<arch>-sha256-<digest>
└── update-state.json
```

On Windows the active executable is `$PI_HOME/bin/rpi.exe` rather than a
symlink into `downloads/`.

On Unix, the installer creates the managed directories (`$PI_HOME`,
`bin/`, `downloads/`) owner-only (`0700`) and writes `update-state.json` and
the install lock as owner-only (`0600`), independent of the caller's umask.
Installed binaries keep their executable mode (`0755`).

Runtime configuration and sessions are stored separately under `<agent-dir>/`
(the upstream pi layout, defaulting to `~/.pi/agent`). The binary location and
the agent directory are independent, so you can point `PI_CODING_AGENT_DIR` at
a different config tree.
