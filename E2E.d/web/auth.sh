#!/usr/bin/env bash
# Web WebSocket-auth E2E lane (playwright-only hard gate).
#
# Spawns the real `rpi --listen` binary with a token file and the loopback
# mock provider (--scenario steering), opens `/web` in a real browser, and
# asserts the rpi-auth.<token> subprotocol contract:
#   1. NO token: the boot auto-connect probe fails SILENTLY (no error toast;
#      the empty-hint explains the requirement) and the pill never reaches
#      `connected` — it settles into `reconnecting`
#   2. WRONG token: an explicit Connect surfaces the "wrong or missing token"
#      ERROR toast and never reaches `connected`
#   3. GOOD token: Connect reaches `connected` and a prompt round-trips
#
# Browser driver: playwright via npm (ephemeral install in the scenario work
# dir) with a system Chrome/Chromium binary or playwright's bundled chromium.
# Missing node, a failed playwright install, or no usable Chromium FAILS the
# lane (exit 1, setup); assertion failures exit 2+.
#
# Usage: bash E2E.d/web/auth.sh [run|list]
set -euo pipefail
SCRIPT_DIR="$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
E2E_DIR="$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd -P)"
# shellcheck source=../lib/common.sh
. "$E2E_DIR/lib/common.sh"
# shellcheck source=lib/fixture.sh
. "$SCRIPT_DIR/lib/fixture.sh"

SCENARIO="web-auth"
TOKEN="web-auth-e2e-token-$$-$(date +%s)"
WRONG_TOKEN="definitely-wrong-token-$$"
SLOW_TAIL="chunk-four-done"

# ---------------------------------------------------------------------------
main() {
    case "${1:-run}" in
        list|--list|--dry-run)
            printf '%s\n' \
                'web-auth - rpi-auth subprotocol: no-token silent probe (never connects), wrong-token error toast, good-token connect + round-trip (playwright, hard gate)'
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
    web_run_playwright "$url" "$evidence" "$root/playwright" "$SCRIPT_DIR/auth_test.mjs" \
        RPI_WRONG_TOKEN="$WRONG_TOKEN" RPI_SLOW_TAIL="$SLOW_TAIL" || pw_status=$?
    if [ "$pw_status" -eq 0 ]; then
        printf 'playwright: no-token silent probe + wrong-token error toast + good-token connect: PASSED\n' | tee "$evidence/playwright-summary.txt"
    elif [ "$pw_status" -eq 1 ]; then
        fail "$SCENARIO: playwright SETUP FAILED (node/chromium/npm unavailable)"
    else
        fail "$SCENARIO: playwright lane failed (exit $pw_status)"
    fi

    web_sanity_http "$url"
    web_finish_lane "$SCENARIO"
}

main "$@"
