#!/usr/bin/env bash
set -euo pipefail
SCRIPT_DIR="$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
printf '%s\n' '[CI deterministic]'
"$SCRIPT_DIR/ci/campaign.sh" list
"$SCRIPT_DIR/ci.sh" list
printf '%s\n' '[Release]'
"$SCRIPT_DIR/release.sh" list
printf '%s\n' '[Nightly/manual live model]'
"$SCRIPT_DIR/nightly.sh" list
