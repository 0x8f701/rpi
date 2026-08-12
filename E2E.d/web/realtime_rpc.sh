#!/usr/bin/env bash
# Web realtime RPC-level start/error/stop regression lane (playwright-only
# hard gate).
#
# Spawns the real `rpi --listen` binary with a token file, live settings
# advertising realtime voice mode (settings.json live.enabled/mode/realtime
# + realtimeBaseUrl pointing at the loopback mock), and the loopback mock
# provider's /v1/realtime/calls endpoint; opens `/web` in a real browser and
# asserts the USER-VISIBLE realtime lifecycle against the REAL Rust proxy
# path (realtime_create_call -> mock) in two phases:
#   ok phase (MOCK_REALTIME_ERROR unset):
#     R1  the mic button enters realtime mode (backend advertises
#         runtimeSettings.live = {enabled, mode: "realtime"}); clicking
#         #mic-btn dispatches realtime_create_call on the WS
#     R2  the Rust realtime proxy reaches the mock with the real JSON
#         create-call request (OpenAI-Alpha: quicksilver=v2 + Bearer
#         persisted in the mock evidence file) — the user click drives the
#         REAL server path, not a client-only fake
#     R3  a successful call renders the live overlay (#realtime-transcript
#         with the "realtime voice" label) and exposes the connection-state
#         bucket (#realtime-conn-state)
#     R5  clicking #mic-btn again while active dispatches realtime_stop and
#         the overlay disappears
#   error phase (MOCK_REALTIME_ERROR=1):
#     R4  the mock rejects /v1/realtime/calls with 500; the page surfaces the
#         user-visible "realtime call failed" toast and the overlay never
#         renders
#
# The in-page RTCPeerConnection is stubbed (addInitScript fake) so the
# lifecycle is deterministic: transport correctness is covered by the
# real-WebRTC lane (realtime_webrtc); this lane proves the start/error/stop
# UI + RPC + proxy wiring.
#
# Browser driver: playwright via npm (ephemeral install) with a system
# Chrome/Chromium binary or playwright's bundled chromium. Missing node, a
# failed playwright install, or no usable Chromium FAILS the lane (exit 1,
# setup); assertion failures exit 2+.
#
# Usage: bash E2E.d/web/realtime_rpc.sh [run|list]
set -euo pipefail
SCRIPT_DIR="$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
E2E_DIR="$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd -P)"
# shellcheck source=../lib/common.sh
. "$E2E_DIR/lib/common.sh"
# shellcheck source=lib/fixture.sh
. "$SCRIPT_DIR/lib/fixture.sh"

SCENARIO="web-realtime-rpc"
TOKEN="web-realtime-rpc-e2e-token-$$-$(date +%s)"

run_phase() {
    local phase="$1" root="$2" evidence="$3" mock_error="$4"
    printf '%s\n' "$TOKEN" >"$root/token"

    # The mock must be up BEFORE the settings file is written: the live
    # realtimeBaseUrl points at the mock port. MOCK_REALTIME_EVIDENCE makes
    # the mock persist the proxy's real request headers (OpenAI-Alpha +
    # Bearer) for R2; MOCK_REALTIME_ERROR=1 makes the endpoint reject with
    # 500 for the R4 error phase.
    local proxy_evidence="$evidence/realtime-proxy-evidence.json"
    if [ "$mock_error" = "1" ]; then
        MOCK_REALTIME_EVIDENCE="$proxy_evidence" MOCK_REALTIME_ERROR=1 web_start_mock "$root" "$evidence"
    else
        MOCK_REALTIME_EVIDENCE="$proxy_evidence" web_start_mock "$root" "$evidence"
    fi
    local port
    port="$(cat "$root/mock-port.txt")"

    # Live realtime settings in the isolated agent dir so the Web listener
    # advertises runtimeSettings.live = {enabled: true, mode: "realtime"} and
    # the composer mic enters realtime mode. realtimeApiKey is a fixture
    # secret written directly to the fixture settings file (never logged).
    cat >"$root/home/.pi/agent/settings.json" <<EOF
{
  "live": {
    "enabled": true,
    "mode": "realtime",
    "realtimeBaseUrl": "http://127.0.0.1:$port",
    "realtimeApiKey": "realtime-rpc-fixture-key",
    "realtimeModel": "gpt-realtime-1.5",
    "voice": "sol",
    "allowInsecure": true
  }
}
EOF

    web_spawn_rpi "$root" "$evidence" "$port"
    local url
    url="$(web_wait_for_listener "$evidence")"
    log "$SCENARIO ($phase): listener at $url"

    local pw_status=0
    web_run_playwright "$url" "$evidence" "$root/playwright-$phase" \
        "$SCRIPT_DIR/realtime_rpc_test.mjs" \
        "RPI_PHASE=$phase" \
        "RPI_PROXY_EVIDENCE=$proxy_evidence" || pw_status=$?
    if [ "$pw_status" -eq 0 ]; then
        web_sanity_http "$url" >/dev/null
    fi
    web_kill_rpi
    web_kill_mock "$root"
    if [ "$pw_status" -eq 0 ]; then
        printf 'playwright (%s): realtime start/error/stop RPC lifecycle PASSED\n' "$phase" | tee "$evidence/playwright-$phase-summary.txt"
    elif [ "$pw_status" -eq 1 ]; then
        fail "$SCENARIO ($phase): playwright SETUP FAILED (node/chromium/npm unavailable)"
    else
        fail "$SCENARIO ($phase): playwright lane failed (exit $pw_status)"
    fi

}

main() {
    case "${1:-run}" in
        list|--list|--dry-run)
            printf '%s\n' \
                'web-realtime-rpc - user-visible realtime start/error/stop against the REAL Rust proxy: #mic-btn dispatches realtime_create_call + the mock records the quicksilver JSON request, live overlay renders ("realtime voice" + conn-state bucket), mock-500 surfaces the "realtime call failed" toast with the overlay down, second click dispatches realtime_stop + overlay gone (PLAYWRIGHT-ONLY, hard-fail)'
            return 0
            ;;
        run|all) ;;
        *) fail "usage: $0 [run|list]" ;;
    esac

    require_rpi
    web_require_browser

    # Phase 1: success lifecycle (start -> overlay -> proxy evidence -> stop).
    local root evidence
    root="$(scenario_workspace "$SCENARIO")"
    evidence="$EVIDENCE_ROOT/$SCENARIO"
    mkdir -p "$root/home/.pi/agent" "$root/workspace"
    run_phase "ok" "$root" "$evidence" "0"

    # Phase 2: error lifecycle (mock 500 -> "realtime call failed" toast,
    # overlay never renders).
    local root2 evidence2
    root2="$(scenario_workspace "$SCENARIO-error")"
    evidence2="$EVIDENCE_ROOT/$SCENARIO-error"
    mkdir -p "$root2/home/.pi/agent" "$root2/workspace"
    run_phase "error" "$root2" "$evidence2" "1"

    web_finish_lane "$SCENARIO"
}

main "$@"
