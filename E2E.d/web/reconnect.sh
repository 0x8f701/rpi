#!/usr/bin/env bash
# Web auto-reconnect E2E lane (playwright-only hard gate).
#
# Spawns the real `rpi --listen` binary with a token file and the loopback
# mock provider (--scenario steering; request 1 streams slowly, request 2 is
# instant), opens `/web` in a real browser, and asserts the reconnect
# contract against a REAL server crash:
#   1. the WS connects and a first prompt round-trips (slow reply)
#   2. the lane KILLS the rpi listener mid-session -> the pill must flip to
#      `reconnecting` on its own (App.tsx scheduleReconnect)
#   3. the lane respawns the listener on the SAME port -> the client
#      auto-reconnects to `connected` without any user action
#   4. the pre-crash transcript survives the reconnect
#   5. a prompt after the reconnect round-trips (request 2, instant reply)
#
# The playwright lane synchronizes with the lane script through two marker
# files in the scenario work dir: reconnect_test.mjs writes
# kill-server.marker, the lane kills + respawns rpi and writes
# server-up.marker.
#
# Browser driver: playwright via npm (ephemeral install in the scenario work
# dir) with a system Chrome/Chromium binary or playwright's bundled chromium.
# Missing node, a failed playwright install, or no usable Chromium FAILS the
# lane (exit 1, setup); assertion failures exit 2+.
#
# Usage: bash E2E.d/web/reconnect.sh [run|list]
set -euo pipefail
SCRIPT_DIR="$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
E2E_DIR="$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd -P)"
# shellcheck source=../lib/common.sh
. "$E2E_DIR/lib/common.sh"
# shellcheck source=lib/fixture.sh
. "$SCRIPT_DIR/lib/fixture.sh"

SCENARIO="web-reconnect"
TOKEN="web-reconnect-e2e-token-$$-$(date +%s)"
FIRST_TAIL="chunk-four-done"     # tail of the FIRST (slow) mock stream
FAST_REPLY="steering-followup-reply"

# ---------------------------------------------------------------------------
main() {
    case "${1:-run}" in
        list|--list|--dry-run)
            printf '%s\n' \
                'web-reconnect - real server kill -> reconnecting pill -> auto-reconnect on same port + transcript survives + post-reconnect round-trip (playwright, hard gate)'
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

    local pw_status=0 pw_pid=0
    web_run_playwright "$url" "$evidence" "$root/playwright" "$SCRIPT_DIR/reconnect_test.mjs" \
        RPI_FIRST_TAIL="$FIRST_TAIL" RPI_FAST_REPLY="$FAST_REPLY" RPI_WORK="$root/playwright" \
        >"$evidence/playwright.out" 2>&1 &
    pw_pid=$!

    # Wait for the test's kill request; respawn the listener on the SAME port.
    local deadline=$((SECONDS + 120))
    while [ ! -f "$root/playwright/kill-server.marker" ] && [ "$SECONDS" -lt "$deadline" ]; do
        if ! kill -0 "$pw_pid" 2>/dev/null; then break; fi
        sleep 0.2
    done
    if [ -f "$root/playwright/kill-server.marker" ]; then
        local listen_port url2
        listen_port="$(printf '%s' "$url" | sed -n 's#http://[^:]*:\([0-9]*\)/web#\1#p')"
        log "$SCENARIO: test requested a server kill — killing rpi (port $listen_port), respawning"
        web_kill_rpi
        sleep 1
        # --continue: resume the same session id so the client keeps the
        # pre-crash transcript (same-id reconnect must not clear).
        WEB_SPAWN_CONTINUE=1 web_spawn_rpi "$root" "$evidence" "$port" "$listen_port" "2"
        url2="$(web_wait_for_listener "$evidence" "2")"
        log "$SCENARIO: listener respawned at $url2"
        touch "$root/playwright/server-up.marker"
    fi

    wait "$pw_pid" || pw_status=$?
    if [ "$pw_status" -eq 0 ]; then
        printf 'playwright: reconnecting pill, auto-reconnect, transcript survival, round-trip: PASSED\n' | tee "$evidence/playwright-summary.txt"
    elif [ "$pw_status" -eq 1 ]; then
        fail "$SCENARIO: playwright SETUP FAILED (node/chromium/npm unavailable)"
    else
        fail "$SCENARIO: playwright lane failed (exit $pw_status)"
    fi

    web_sanity_http "$url"
    web_finish_lane "$SCENARIO"
}

main "$@"
