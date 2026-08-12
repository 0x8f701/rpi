#!/usr/bin/env bash
# Web realtime WebRTC protocol-level E2E lane (playwright-only hard gate).
#
# NOT the FakePeerConnection coverage path. This lane exercises a REAL
# Chromium RTCPeerConnection loopback (two real peers in one page: real
# getUserMedia audio track, real offer/answer, real ICE candidate gathering,
# real 'oai-events' RTCDataChannel open + message round-trip, real remote audio
# track arrival) AND the REAL src/realtime.ts helpers (waitForIceGatheringComplete,
# classifyRealtimeConnectionState, setupRealtimeCall) bundled into the page —
# proving the ICE-gather-then-POST fix works against the platform WebRTC stack,
# not just mock peers.
#
# External CLIProxy/openai realtime endpoint is NOT exercised here: that requires
# a secret API key + HTTPS secure context + a reachable remote endpoint, which
# this lane cannot access without secret material. The loopback is the
# protocol-level executable substitute: it drives the SAME browser WebRTC +
# SDP/datachannel/audio-track code path the production call uses, minus the
# remote signaling hop.
#
# Browser driver: playwright via npm (ephemeral install in the scenario work
# dir) with a system Chrome/Chromium binary. Chrome is launched with
# --use-fake-device-for-media-stream (synthetic mic) + --use-fake-ui-for-media-
# stream (auto-approve permission) + --autoplay-policy=no-user-gesture-required
# (remote audio play). Missing node, a failed playwright install, no usable
# Chromium, or a WebRTC API failure FAILS the lane (exit 1 setup / 2+ assertion).
#
# Usage: bash E2E.d/web/realtime_webrtc.sh [run|list]
set -euo pipefail
SCRIPT_DIR="$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
E2E_DIR="$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd -P)"
REPO_ROOT="$(CDPATH= cd -- "$E2E_DIR/.." && pwd -P)"
# shellcheck source=../lib/common.sh
. "$E2E_DIR/lib/common.sh"
# shellcheck source=lib/fixture.sh
. "$SCRIPT_DIR/lib/fixture.sh"

SCENARIO="web-realtime-webrtc"
WEB_DIR="$REPO_ROOT/crates/pi-cli/web"

# ---------------------------------------------------------------------------
main() {
    case "${1:-run}" in
        list|--list|--dry-run)
            printf '%s\n' \
                'web-realtime-webrtc - REAL Chromium RTCPeerConnection loopback: ICE gather wait, oai-events datachannel open + round-trip, remote audio track, setupRealtimeCall end-to-end (playwright, hard gate; no FakePeerConnection)'
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

    # The WebRTC loopback runs in the page JS context and does NOT touch the
    # WS backend or the mock provider, but the listener needs a resolvable
    # model to boot (--model user-steering/mock), so start the loopback mock
    # purely to keep the listener alive serving /web.
    port="$(web_start_mock "$root" "$evidence")"
    WEB_SPAWN_TOKENLESS=1 web_spawn_rpi "$root" "$evidence" "$port"
    url="$(web_wait_for_listener "$evidence")"
    log "$SCENARIO: tokenless listener at $url (page context for the WebRTC loopback)"

    # Bundle src/realtime.ts into an IIFE exposing window.__rtHelpers so the
    # page.evaluate loopback can call the REAL helpers (waitForIceGatheringComplete,
    # classifyRealtimeConnectionState, setupRealtimeCall, REALTIME_EVENT_CHANNEL,
    # REALTIME_ICE_GATHER_TIMEOUT_MS) against real RTCPeerConnections. realtime.ts
    # has no DOM/WS imports, so it bundles standalone with esbuild.
    local work="$root/playwright"
    mkdir -p "$work"
    local helpers="$work/rt-helpers.js"
    if ! (cd "$WEB_DIR" && npx esbuild src/realtime.ts --bundle --format=iife \
            --global-name=__rtHelpers --outfile="$helpers" >/dev/null 2>&1); then
        fail "$SCENARIO: bundling src/realtime.ts -> rt-helpers.js failed (esbuild unavailable in $WEB_DIR)"
    fi
    [ -s "$helpers" ] || fail "$SCENARIO: rt-helpers.js bundle is empty"

    local pw_status=0
    web_run_playwright "$url" "$evidence" "$work" "$SCRIPT_DIR/realtime_webrtc_test.mjs" \
        RPI_RT_HELPERS="$helpers" || pw_status=$?
    if [ "$pw_status" -eq 0 ]; then
        printf 'playwright: REAL RTCPeerConnection loopback + ICE-gather fix + datachannel/audio: PASSED\n' | tee "$evidence/playwright-summary.txt"
    elif [ "$pw_status" -eq 1 ]; then
        fail "$SCENARIO: playwright SETUP FAILED (node/chromium/npm unavailable)"
    else
        fail "$SCENARIO: playwright lane failed (exit $pw_status)"
    fi

    web_sanity_http "$url"
    web_finish_lane "$SCENARIO"
}

main "$@"