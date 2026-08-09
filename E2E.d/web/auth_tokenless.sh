#!/usr/bin/env bash
# Web WebSocket tokenless-listener E2E lane (playwright-only hard gate).
#
# Spawns the real `rpi --listen` binary WITHOUT a token file (the tokenless
# policy: loopback accepts browser connections directly) and the loopback mock
# provider (--scenario steering), opens `/web` in a real browser, and asserts
# the tokenless contract:
#   1. The boot auto-connect with an EMPTY token reaches `connected` (the page
#      never asks for a token) and no error toast appears.
#   2. A prompt round-trips over the tokenless connection.
#
# This is the complement of auth.sh: auth.sh runs a tokened listener and
# asserts the no-token probe fails silently + wrong-token toast + good-token
# connect; this lane runs a tokenless listener and asserts the empty-token boot
# connects.
#
# Browser driver: playwright via npm (ephemeral install in the scenario work
# dir) with a system Chrome/Chromium binary or playwright's bundled chromium.
# Missing node, a failed playwright install, or no usable Chromium FAILS the
# lane (exit 1, setup); assertion failures exit 2+.
#
# Usage: bash E2E.d/web/auth_tokenless.sh [run|list]
set -euo pipefail
SCRIPT_DIR="$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
E2E_DIR="$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd -P)"
# shellcheck source=../lib/common.sh
. "$E2E_DIR/lib/common.sh"
# shellcheck source=lib/fixture.sh
. "$SCRIPT_DIR/lib/fixture.sh"

SCENARIO="web-auth-tokenless"
SLOW_TAIL="chunk-four-done"

# ---------------------------------------------------------------------------
main() {
    case "${1:-run}" in
        list|--list|--dry-run)
            printf '%s\n' \
                'web-auth-tokenless - tokenless listener: empty-token boot auto-connect reaches connected + prompt round-trip (playwright, hard gate)'
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

    port="$(web_start_mock "$root" "$evidence")"
    # WEB_SPAWN_TOKENLESS=1 makes web_spawn_rpi omit --listen-token-file so the
    # listener runs the tokenless policy (loopback accepts browsers).
    WEB_SPAWN_TOKENLESS=1 web_spawn_rpi "$root" "$evidence" "$port"
    url="$(web_wait_for_listener "$evidence")"
    log "$SCENARIO: tokenless listener at $url"

    local pw_status=0
    web_run_playwright "$url" "$evidence" "$root/playwright" "$SCRIPT_DIR/auth_tokenless_test.mjs" \
        RPI_SLOW_TAIL="$SLOW_TAIL" || pw_status=$?
    if [ "$pw_status" -eq 0 ]; then
        printf 'playwright: tokenless empty-token boot connect + round-trip: PASSED\n' | tee "$evidence/playwright-summary.txt"
    elif [ "$pw_status" -eq 1 ]; then
        fail "$SCENARIO: playwright SETUP FAILED (node/chromium/npm unavailable)"
    else
        fail "$SCENARIO: playwright lane failed (exit $pw_status)"
    fi

    web_sanity_http "$url"
    web_finish_lane "$SCENARIO"
}

main "$@"