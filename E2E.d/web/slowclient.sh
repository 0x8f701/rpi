#!/usr/bin/env bash
# Web slow-client E2E lane (playwright-only hard gate).
#
#   Spawns the real `rpi --listen` binary with a token file and the loopback
# mock provider (--scenario slowclient: content-routed delta bursts and heavy
# markdown/mermaid finals), opens /web in a real browser, and measures/asserts
# the slow-client contract:
#   1. calibration: a small burst drains with bounded per-message handler
#      cost, no long tasks, no spurious close.
#   2. heavy-transcript amplification: with final KaTeX/mermaid messages in
#      the transcript, a delta burst must not balloon main-thread work (the
#      hot path must not re-render the whole transcript per delta).
#   3. controlled reader stall: a synthetic main-thread busy loop overlapping
#      a sustained delta flood must NOT produce a false 1008
#      ("client is not reading messages") for a TRANSIENT (<=4s) stall; a
#      SUSTAINED no-read stall must still surface the real 1008 with the
#      server's reason. The lane records WS close code/reason,
#      PerformanceObserver long tasks, wire-receive vs JS-handler message
#      timestamps, and the renderer queue gap (JS-handler lag).
#   4. accurate close cause: the 1008 close toast carries the server's reason
#      and the pending command is drained in onclose — no mislabeled
#      "send failed: connection replaced"/"command timed out" toast.
#
# The 1008 toast is NEVER suppressed: the lane asserts the app surfaces any
# non-1000 close (including a genuine 1008) verbatim.
#
# Browser driver: playwright via npm (ephemeral install in the scenario work
# dir) with a system Chrome/Chromium binary or playwright's bundled chromium.
# Missing node, a failed playwright install, or no usable Chromium FAILS the
# lane (exit 1, setup); assertion failures exit 2+.
#
# Usage: bash E2E.d/web/slowclient.sh [run|list]
set -euo pipefail
SCRIPT_DIR="$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
E2E_DIR="$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd -P)"
# shellcheck source=../lib/common.sh
. "$E2E_DIR/lib/common.sh"
# shellcheck source=lib/fixture.sh
. "$SCRIPT_DIR/lib/fixture.sh"

SCENARIO="web-slowclient"
MOCK_SCENARIO="slowclient"
TOKEN="web-slowclient-e2e-token-$$-$(date +%s)"

# ---------------------------------------------------------------------------
main() {
    case "${1:-run}" in
        list|--list|--dry-run)
            printf '%s\n' \
                'web-slowclient - slow-client contract: burst calibration, heavy-transcript delta amplification, controlled main-thread stall vs delta flood (no false 1008), sustained no-read 1008 + accurate cause, 1008 toast never hidden (playwright, hard gate)'
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
    web_run_playwright "$url" "$evidence" "$root/playwright" "$SCRIPT_DIR/slowclient_test.mjs" || pw_status=$?
    if [ "$pw_status" -eq 0 ]; then
        printf 'playwright: slow-client contract: PASSED\n' | tee "$evidence/playwright-summary.txt"
    elif [ "$pw_status" -eq 1 ]; then
        fail "$SCENARIO: playwright SETUP FAILED (node/chromium/npm unavailable)"
    else
        fail "$SCENARIO: playwright lane failed (exit $pw_status)"
    fi

    web_sanity_http "$url"
    web_finish_lane "$SCENARIO"
}

main "$@"
