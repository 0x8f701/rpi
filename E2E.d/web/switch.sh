#!/usr/bin/env bash
# Web model + thinking-level switch E2E lane (playwright-only hard gate).
#
# Spawns the real `rpi --listen` binary with a token file and the loopback
# mock provider (--scenario steering; the fixture registers a REASONING mock
# model plus a SECOND model so both header selects are switchable), opens
# `/web` in a real browser, and asserts:
#   1. #model-select lists both fixture models and reflects the current one
#   2. switching the model round-trips (set_model -> get_state -> select)
#   3. #thinking-select is enabled for the reasoning model and switching
#      round-trips (set_thinking_level -> get_state -> select)
#   4. the session still round-trips after the switches
#
# Browser driver: playwright via npm (ephemeral install in the scenario work
# dir) with a system Chrome/Chromium binary or playwright's bundled chromium.
# Missing node, a failed playwright install, or no usable Chromium FAILS the
# lane (exit 1, setup); assertion failures exit 2+.
#
# Usage: bash E2E.d/web/switch.sh [run|list]
set -euo pipefail
SCRIPT_DIR="$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
E2E_DIR="$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd -P)"
# shellcheck source=../lib/common.sh
. "$E2E_DIR/lib/common.sh"
# shellcheck source=lib/fixture.sh
. "$SCRIPT_DIR/lib/fixture.sh"

SCENARIO="web-switch"
TOKEN="web-switch-e2e-token-$$-$(date +%s)"
SLOW_TAIL="chunk-four-done"

# ---------------------------------------------------------------------------
main() {
    case "${1:-run}" in
        list|--list|--dry-run)
            printf '%s\n' \
                'web-switch - model + thinking-level switch (set_model / set_thinking_level round-trips) with a reasoning dual-model fixture + post-switch round-trip (playwright, hard gate)'
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

    MOCK_REASONING=1 MOCK_SECOND_MODEL=1 port="$(web_start_mock "$root" "$evidence")"
    web_spawn_rpi "$root" "$evidence" "$port"
    url="$(web_wait_for_listener "$evidence")"
    log "$SCENARIO: listener at $url"

    local pw_status=0
    web_run_playwright "$url" "$evidence" "$root/playwright" "$SCRIPT_DIR/switch_test.mjs" \
        RPI_SLOW_TAIL="$SLOW_TAIL" || pw_status=$?
    if [ "$pw_status" -eq 0 ]; then
        printf 'playwright: model + thinking-level switch round-trips: PASSED\n' | tee "$evidence/playwright-summary.txt"
    elif [ "$pw_status" -eq 1 ]; then
        fail "$SCENARIO: playwright SETUP FAILED (node/chromium/npm unavailable)"
    else
        fail "$SCENARIO: playwright lane failed (exit $pw_status)"
    fi

    web_sanity_http "$url"
    web_finish_lane "$SCENARIO"
}

main "$@"
