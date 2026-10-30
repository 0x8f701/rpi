# Update safety

The installer and native self-updater use GitHub release metadata, platform archives, and `SHA256SUMS`. Downloading a release and activating its executable are separate stages.

## Commands

```sh
rpi update
rpi update --self
rpi update --extensions
rpi update --all
rpi update --models
```

With no target, update selects the managed rpi installation. `--extensions` selects configured extensions; `--all` selects both. `--models` refreshes dynamic model catalogs. `--extension SOURCE` selects one configured extension. `--force` applies only when self-update is included.

## Activation

Self-update downloads the selected platform asset, verifies its checksum, smoke-checks the staged executable, and activates it atomically. Download files are created as new files and synced before activation. Platform-specific activation handles Windows executable replacement separately from Unix installation.

The Unix installer stores versioned downloads and activates a link under the install root's `bin` directory. A failed stage must not become the active partial binary. Inspect the reported error rather than manually copying an unverified archive over the executable.

## Networking and notices

`RPI_OFFLINE=1|true|yes` disables updater networking. `RPI_SKIP_VERSION_CHECK` suppresses only the nonfatal interactive startup notice. These have different scope.

`RPI_UPDATE_BASE_URL` selects a GitHub-Releases-shaped metadata endpoint. Asset URLs are constrained by the operator transport rather than trusted as arbitrary download authorities from metadata. Installer `GITHUB_TOKEN` authentication is limited to its fixed GitHub API endpoint, not forwarded to asset hosts or arbitrary custom endpoints.

## Installation root

The installer uses `RPI_HOME`, ordinarily the user's `.rpi` directory. Runtime `PI_CODING_AGENT_DIR` and profiles control configuration location; do not assume they select a managed binary installation.

The Unix installer explicitly rejects `PI_HOME` and `PI_UPDATE_BASE_URL`; use the `RPI_*` installer variables instead.

## Safety

Checksum verification establishes consistency with the published manifest, not independence from the publisher. Use trusted release endpoints and review update errors. Do not include access tokens or private endpoint values in diagnostic reports. See [installation](../introduction/install.md).
