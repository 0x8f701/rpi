#!/usr/bin/env bash
# Web failed-turn transcript E2E lane — real Chromium regression that a turn
# failing at the model transport layer (mock HTTP 500 seed 'web turn error')
# surfaces its persisted errorMessage in the transcript: live row, failed
# toast, composer back to idle, the row survives a reload refill, and the
# mobile 390x844 width never overflows. PLAYWRIGHT-ONLY hard gate.
#
# Usage: bash E2E.d/web/turn_error.sh [run|list]
set -euo pipefail
SCRIPT_DIR="$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
E2E_DIR="$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd -P)"
# shellcheck source=../lib/common.sh
. "$E2E_DIR/lib/common.sh"
# shellcheck source=lib/fixture.sh
. "$SCRIPT_DIR/lib/fixture.sh"

SCENARIO="web-turn-error"
TOKEN="web-turn-error-e2e-token-$$-$(date +%s)"

main() {
    case "${1:-run}" in
        list|--list|--dry-run)
            printf '%s\n' \
                'web-turn-error - failed turn renders its errorMessage row (live + restored + mobile, failed toast, composer idle) (PLAYWRIGHT-ONLY, hard-fail)'
            return 0
            ;;
        run|all) ;;
        *) fail "usage: $0 [run|list]" ;;
    esac

    require_rpi
    require_cmd node
    require_cmd python3

    local root evidence port url pw_status=0
    root="$(scenario_workspace "$SCENARIO")"
    evidence="$EVIDENCE_ROOT/$SCENARIO"
    printf '%s\n' "$TOKEN" >"$root/token"

    port="$(web_start_mock "$root" "$evidence")"
    web_spawn_rpi "$root" "$evidence" "$port"
    url="$(web_wait_for_listener "$evidence")"
    log "web.$SCENARIO: listener at $url (token in $root/token)"

    web_run_playwright "$url" "$evidence" "$root/playwright" "$SCRIPT_DIR/turn_error_test.mjs" \
        RPI_EVIDENCE="$evidence" || pw_status=$?
    if [ "$pw_status" -eq 0 ]; then
        printf 'playwright: failed-turn transcript contracts: PASSED\n' | tee "$evidence/playwright-summary.txt"
    elif [ "$pw_status" -eq 1 ]; then
        fail "web.$SCENARIO: playwright SETUP FAILED (node/chromium/npm unavailable)"
    else
        fail "web.$SCENARIO: playwright lane failed (exit $pw_status)"
    fi

    web_sanity_http "$url"
    web_finish_lane "$SCENARIO"
}

main "$@"
