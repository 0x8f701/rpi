# Installation

The release archive contains the compiled `rpi` executable; ordinary installation does not require Rust or a source checkout.

## Linux and macOS

```sh
curl -fsSL https://raw.githubusercontent.com/0x8f701/rpi/master/install.sh | sh
```

The installer selects x86-64 or AArch64 Linux/macOS assets, downloads the release archive, verifies it against `SHA256SUMS`, and activates the binary. Its ordinary install root is the user's `.rpi` directory, with `bin/rpi` selecting a versioned download. `RPI_HOME` overrides that install root.

For reproducible installation, download an installer from a reviewed release tag and pass `--version` with that release version. Do not mix an unreviewed moving installer with a claim of fully pinned installation.

The Unix installer rejects unsupported operating systems/architectures and obsolete `PI_HOME` or `PI_UPDATE_BASE_URL` installer variables. Use `RPI_HOME` and `RPI_UPDATE_BASE_URL` instead.

## Windows

Use the repository's PowerShell installer:

```powershell
irm https://raw.githubusercontent.com/0x8f701/rpi/master/install.ps1 | iex
```

Windows does not provide the Linux/macOS confinement backends. Confined modes are rejected; `danger-full-access` runs with host authority. Read [sandbox confinement](../reference/sandbox-isolation.md) before running untrusted code.

## Verify and configure

Open a new terminal if installation changed your executable search path, then run:

```sh
rpi --version
rpi setup
rpi doctor
rpi login
rpi models
```

Setup locates configuration; doctor reports diagnostics; login configures a provider. Installation and provider access are separate requirements. See [quick start](quickstart.md).

## Source builds

Maintainers can build from a checkout with the workspace's Rust 1.88 toolchain or newer compatible toolchain:

```sh
cargo +1.88.0 build -p pi-cli --release
```

The executable is `target/release/rpi`. Build the current Web bundle through the repository's Web build instructions when changing embedded browser assets; a Rust compile alone does not establish that newly edited browser source was bundled.

Do not use debug-profile product execution for verification in this repository. Build/test coordination follows the repository rules, particularly when other writers are changing participating files.

## Updates and safety

`rpi update --self` updates a managed installation after download, checksum verification, and staged executable checking. Protect release endpoints and installation files. A checksum validates consistency with the publisher's manifest, not independent trust in the publisher. See [update safety](../reference/update.md).
