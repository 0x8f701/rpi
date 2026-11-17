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
#   3. controlled renderer stall: a synthetic main-thread busy loop
#      overlapping a delta flood must NOT produce a false 1008. The 2.5s,
#      4s, and 7s stalls are all below the 15s outbound grace and are JS
#      stalls, not TCP no-read. Each turn completes; the 7s flood must
#      preserve exact salted content and order. Dead-client eviction is
#      owned by the Rust TCP/stall tests, not this browser lane.
#   4. close surface: after the floods, the test opens /web through a
#      test-owned ws reverse proxy, holds the real side_chat_list response
#      with the browser's sent command id, and sends a server 1008 close.
#      The lane proves that response stayed undelivered through the close
#      handler and that the toast carries code 1008 and the controlled
#      reason, with no timeout or connection-replaced toast. It does NOT
#      observe the product pending-registry reject; that remains the
#      existing pending unit test. This is not dead-client eviction.
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
                'web-slowclient - slow-client contract: burst calibration, heavy-transcript delta amplification, renderer stalls including 7s complete without false 1008 and with exact salted content, controlled server 1008 close while side_chat_list is pending, accurate close cause (playwright, hard gate)'
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
