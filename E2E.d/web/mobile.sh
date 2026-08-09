#!/usr/bin/env bash
# Web mobile-viewport E2E lane (playwright-only hard gate).
#
# Spawns the real `rpi --listen` binary with a token file and the loopback
# mock provider (--scenario steering; request 1 streams slowly), opens `/web`
# in a real browser at a 375×667 phone viewport, and asserts:
#   1. the core flow works at phone width: connect, prompt round-trip, panel
#      open
#   2. no horizontal page overflow (page itself, and with the drawer open)
#   3. the drawer is full-screen width (== viewport)
#   4. the composer sits above the fold (bottom <= innerHeight)
#   5. shell touch targets are >= 44px (send/connect/abort/panel toggle)
#   6. #thinking-select is hidden at phone width (CSS media query)
#
# Browser driver: playwright via npm (ephemeral install in the scenario work
# dir) with a system Chrome/Chromium binary or playwright's bundled chromium.
# Missing node, a failed playwright install, or no usable Chromium FAILS the
# lane (exit 1, setup); assertion failures exit 2+. Viewport emulation is
# playwright-only, so there is no non-playwright fallback.
#
# Usage: bash E2E.d/web/mobile.sh [run|list]
set -euo pipefail
SCRIPT_DIR="$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
E2E_DIR="$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd -P)"
# shellcheck source=../lib/common.sh
. "$E2E_DIR/lib/common.sh"
# shellcheck source=lib/fixture.sh
. "$SCRIPT_DIR/lib/fixture.sh"

SCENARIO="web-mobile"
TOKEN="web-mobile-e2e-token-$$-$(date +%s)"
SLOW_TAIL="chunk-four-done"

# ---------------------------------------------------------------------------
main() {
    case "${1:-run}" in
        list|--list|--dry-run)
            printf '%s\n' \
                'web-mobile - 375x667 viewport: core flow, no horizontal overflow, full-screen drawer, composer on-screen, 44px touch targets, thinking-select hidden (playwright, hard gate)'
            return 0
            ;;
        run|all) ;;
        *) fail "usage: $0 [run|list]" ;;
    esac

    require_rpi
    web_require_browser

    local root evidence port url
    root="$(scenario_workspace "$SCENARIO")"
    evidence="$EVIDENCE_ROOT/$SCENARIO"
    printf '%s\n' "$TOKEN" >"$root/token"

    port="$(web_start_mock "$root" "$evidence")"
    web_spawn_rpi "$root" "$evidence" "$port"
    url="$(web_wait_for_listener "$evidence")"
    log "$SCENARIO: listener at $url"

    local pw_status=0
    web_run_playwright "$url" "$evidence" "$root/playwright" "$SCRIPT_DIR/mobile_test.mjs" \
        RPI_SLOW_TAIL="$SLOW_TAIL" || pw_status=$?
    if [ "$pw_status" -eq 0 ]; then
        printf 'playwright: mobile core flow + viewport contract: PASSED\n' | tee "$evidence/playwright-summary.txt"
    elif [ "$pw_status" -eq 1 ]; then
        fail "$SCENARIO: playwright SETUP FAILED (node/chromium/npm unavailable)"
    else
        fail "$SCENARIO: playwright lane failed (exit $pw_status)"
    fi

    web_sanity_http "$url"
    web_finish_lane "$SCENARIO"
}

main "$@"
