#!/usr/bin/env bash
# Web abort-semantics E2E lane (playwright-only hard gate).
#
# Spawns the real `rpi --listen` binary with a token file and the loopback
# mock provider (--scenario steering; request 1 streams slowly over ~3.6s),
# opens `/web` in a real browser, and asserts the B1/B2 abort contract:
#   1. B1 regression — aborting EARLY (the instant the first delta lands)
#      PRESERVES the streamed text in the transcript; the final chunks of the
#      slow stream never render.
#   2. B2 regression — abort surfaces a NEUTRAL toast ("run aborted") with no
#      error styling; no error toast appears alongside it.
#   3. the composer recovers and the next prompt round-trips.
#
# Browser driver: playwright via npm (ephemeral install in the scenario work
# dir) with a system Chrome/Chromium binary or playwright's bundled chromium.
# Missing node, a failed playwright install, or no usable Chromium FAILS the
# lane (exit 1, setup); assertion failures exit 2+.
#
# Usage: bash E2E.d/web/abort.sh [run|list]
set -euo pipefail
SCRIPT_DIR="$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
E2E_DIR="$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd -P)"
# shellcheck source=../lib/common.sh
. "$E2E_DIR/lib/common.sh"
# shellcheck source=lib/fixture.sh
. "$SCRIPT_DIR/lib/fixture.sh"

SCENARIO="web-abort"
TOKEN="web-abort-e2e-token-$$-$(date +%s)"
SLOW_PREFIX="steer-1-"            # first chunk of the slow mock stream
ABORTED_TAIL="-done"              # never rendered when the abort cuts the stream
FAST_REPLY="steering-followup-reply"

# ---------------------------------------------------------------------------
main() {
    case "${1:-run}" in
        list|--list|--dry-run)
            printf '%s\n' \
                'web-abort - B1 early-abort preserves streamed text + B2 neutral "run aborted" toast + composer recovery (playwright, hard gate)'
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
    web_run_playwright "$url" "$evidence" "$root/playwright" "$SCRIPT_DIR/abort_test.mjs" \
        RPI_SLOW_PREFIX="$SLOW_PREFIX" RPI_ABORTED_TAIL="$ABORTED_TAIL" RPI_FAST_REPLY="$FAST_REPLY" || pw_status=$?
    if [ "$pw_status" -eq 0 ]; then
        printf 'playwright: early-abort preserves text + neutral toast + recovery: PASSED\n' | tee "$evidence/playwright-summary.txt"
    elif [ "$pw_status" -eq 1 ]; then
        fail "$SCENARIO: playwright SETUP FAILED (node/chromium/npm unavailable)"
    else
        fail "$SCENARIO: playwright lane failed (exit $pw_status)"
    fi

    web_sanity_http "$url"
    web_finish_lane "$SCENARIO"
}

main "$@"
