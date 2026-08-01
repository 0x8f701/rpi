#!/usr/bin/env bash
set -euo pipefail
SCRIPT_DIR="$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
. "$SCRIPT_DIR/../lib/common.sh"
case "${1:-list}" in
    list|--list|--dry-run) printf '%s\n' 'release.archive-fixture - synthesize all five archive shapes and exercise inventory/checksum/member validation'; exit 0 ;;
    run) ;;
    *) fail "usage: $0 [run|list|--dry-run]" ;;
esac
prepare_roots; require_rpi; require_cmd python3
version="$(isolated_rpi_timeout 30 "$WORK_ROOT/home" "$WORK_ROOT" --version | sed -n 's/^rpi //p')"
[ -n "$version" ] || fail "could not parse rpi version"
dist="$WORK_ROOT/archive-fixture"
run_with_timeout 240 python3 "$E2E_DIR/lib/build_release_fixture.py" --version "$version" --rpi "$RPI_BIN" --license "$REPO_ROOT/LICENSE" --output "$dist"
"$SCRIPT_DIR/archive-inventory.sh" run "$version" "$dist" > "$EVIDENCE_ROOT/archive-fixture.log" 2>&1
printf 'release archive fixture passed\nevidence=%s\n' "$EVIDENCE_ROOT/archive-fixture.log"
