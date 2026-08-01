#!/usr/bin/env bash
set -euo pipefail
SCRIPT_DIR="$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
list() {
    "$SCRIPT_DIR/release/archive-inventory.sh" list
    "$SCRIPT_DIR/release/archive-fixture-smoke.sh" list
    "$SCRIPT_DIR/release/install-self-update.sh" list
}
case "${1:-list}" in
    list|--list|--dry-run) list ;;
    archive-fixture) "$SCRIPT_DIR/release/archive-fixture-smoke.sh" run ;;
    install-smoke) "$SCRIPT_DIR/release/install-self-update.sh" run ;;
    lock-smoke) RUN_LOCK_TIMEOUT=1 "$SCRIPT_DIR/release/install-self-update.sh" run ;;
    archives)
        [ "$#" -eq 3 ] || { printf 'usage: %s archives VERSION DIST_DIR\n' "$0" >&2; exit 2; }
        "$SCRIPT_DIR/release/archive-inventory.sh" run "$2" "$3"
        ;;
    run)
        "$SCRIPT_DIR/release/install-self-update.sh" run
        if [ "${RELEASE_FIXTURE_ARCHIVES:-0}" = 1 ]; then
            "$SCRIPT_DIR/release/archive-fixture-smoke.sh" run
        fi
        if [ -n "${RELEASE_VERSION:-}" ] && [ -n "${RELEASE_DIST_DIR:-}" ]; then
            "$SCRIPT_DIR/release/archive-inventory.sh" run "$RELEASE_VERSION" "$RELEASE_DIST_DIR"
        else
            printf 'release.sh: archive inventory skipped; set RELEASE_VERSION and RELEASE_DIST_DIR\n'
        fi
        ;;
    *) printf 'usage: %s [list|--dry-run|archive-fixture|install-smoke|lock-smoke|archives VERSION DIST_DIR|run]\n' "$0" >&2; exit 2 ;;
esac
