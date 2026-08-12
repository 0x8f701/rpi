#!/usr/bin/env bash
# Web ready-gate E2E lane (playwright-only hard gate).
#
# Reproduces the mount-before-WebSocket-OPEN regression and verifies the
# generation-safe bounded ready gate (crates/pi-cli/web/src/socket.ts ReadyGate
# + App.tsx waitForReady/notifyOpen + SessionSidebar/SessionPanel gated load):
#   1. a real `rpi --listen` serves /web (the freshly built dist via
#      RPI_WEB_DEV_DIR, so no Rust rebuild is needed) against the loopback
#      `sessions` mock provider
#   2. a real Chromium opens /web with an in-page WebSocket shim that HOLDS
#      every socket open (shadows readyState to CONNECTING, dispatches the
#      App's onopen only when the test releases it) so the sidebar/session-
#      panel mount BEFORE the socket reaches OPEN
#   3. assertions: the sidebar + session panel finally load the primary
#      session row, an active New click during the disconnect fails fast, a
#      simulated reconnect (close + delayed reopen) reloads the sidebar, and
#      the page NEVER contains `load failed: not connected`
#
# Browser driver: playwright via npm (ephemeral install in the scenario work
# dir) with a system Chrome/Chromium binary or playwright's bundled chromium.
# Missing node, a failed playwright install, or no usable Chromium FAILS the
# lane (exit 1, setup); assertion failures exit 2+.
#
# Usage: bash E2E.d/web/readygate.sh [run|list]
set -euo pipefail
SCRIPT_DIR="$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
E2E_DIR="$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd -P)"
# shellcheck source=../lib/common.sh
. "$E2E_DIR/lib/common.sh"
# shellcheck source=lib/fixture.sh
. "$SCRIPT_DIR/lib/fixture.sh"

SCENARIO="web-readygate"
TOKEN="web-readygate-e2e-token-$$-$(date +%s)"
# Optional settle ms applied by the test ONLY after it releases a held open
# (RPI_OPEN_DELAY=0 default: the test drives the open via explicit
# hold/release, so no fixed delay is needed; a non-zero value just slows the
# post-release assertions for debugging).
OPEN_DELAY="${RPI_OPEN_DELAY:-0}"

# ---------------------------------------------------------------------------
main() {
    case "${1:-run}" in
        run) ;;
        list) printf '%s\n' "$SCENARIO"; exit 0 ;;
        *) printf 'usage: %s [run|list]\n' "$0" >&2; exit 1 ;;
    esac

    require_rpi
    web_require_browser

    local root evidence port url web_dist
    root="$(scenario_workspace "$SCENARIO")"
    evidence="$EVIDENCE_ROOT/$SCENARIO"
    printf '%s\n' "$TOKEN" >"$root/token"

    # Serve the freshly built web dist from disk so the gate fix is exercised
    # WITHOUT rebuilding the Rust binary (build.rs embeds dist at compile time;
    # RPI_WEB_DEV_DIR overrides the embedded copy for frontend iteration).
    # Standalone runs use the checked-in bundle. Coverage runs instead inherit
    # the instrumented RPI_WEB_DEV_DIR exported by coverage.sh; overriding it
    # here would serve the minified bundle (no inline source map) and make this
    # lane's V8 payload unconvertible, failing the coverage hard gate.
    if [ -z "${RPI_COVERAGE_DIR:-}" ]; then
        web_dist="$REPO_ROOT/crates/pi-cli/web/dist"
        [ -f "$web_dist/index.html" ] || fail "$SCENARIO: built web dist not found: $web_dist/index.html (run 'npm run build' in crates/pi-cli/web)"
        export RPI_WEB_DEV_DIR="$web_dist"
    fi

    # The `sessions` mock scenario exposes a primary session row at boot so the
    # sidebar visibly loads after the delayed open (mirrors sessions_test T0.3).
    MOCK_SCENARIO=sessions port="$(web_start_mock "$root" "$evidence")"
    web_spawn_rpi "$root" "$evidence" "$port"
    url="$(web_wait_for_listener "$evidence")"
    log "$SCENARIO: listener at $url (post-release settle ${OPEN_DELAY}ms)"

    local pw_status=0
    web_run_playwright "$url" "$evidence" "$root/playwright" "$SCRIPT_DIR/readygate_test.mjs" \
        RPI_OPEN_DELAY="$OPEN_DELAY" \
        >"$evidence/playwright.out" 2>&1 || pw_status=$?

    web_kill_rpi
    web_kill_mock "$root"

    if [ "$pw_status" -eq 0 ]; then
        web_lane_report "$root/report" "$SCENARIO" PASS "$evidence"
        log "$SCENARIO: PASSED (mount-before-OPEN gate + active fail-fast + reconnect reload)"
    else
        web_lane_report "$root/report" "$SCENARIO" FAIL "$evidence"
        if [ "$pw_status" -eq 1 ]; then
            fail "$SCENARIO: playwright SETUP FAILED (node/chromium/npm unavailable)"
        fi
        fail "$SCENARIO: assertion FAILED (see $evidence/playwright.out)"
    fi

    web_finish_lane "$SCENARIO"
}

main "$@"