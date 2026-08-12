#!/usr/bin/env bash
# Web hold-to-talk STT RPC lane (playwright-only hard gate).
#
# Spawns the real `rpi --listen` binary with a token file, live settings
# advertising hold-to-talk STT voice (settings.json live.enabled/mode/stt +
# sttBaseUrl pointing at the loopback mock), and the mock provider's
# /v1/audio/transcriptions endpoint; opens `/web` in a real browser and
# asserts the USER-VISIBLE STT lifecycle against the REAL Rust proxy
# (stt_transcribe -> mock) in two phases:
#   ok phase (MOCK_STT_ERROR unset):
#     S1  a synthetic-mic press dispatches stt_transcribe on the WS with
#         ONLY {audioBase64, mimeType: "audio/wav"} — no URL, no key
#     S2  the Rust proxy reaches the mock /v1/audio/transcriptions with the
#         server-held Bearer + multipart WAV + model (evidence is metadata
#         only: authPresent/contentType/file/wav/model — never the key or
#         the audio body)
#     S3  the returned transcript lands in the composer (#prompt-input)
#     S4  the fixture key appears nowhere in the page DOM or the evidence
#   error phase (MOCK_STT_ERROR=1):
#     S5  the mock 500 surfaces the bounded "transcription failed" toast and
#         no transcript lands in the composer
#
# The in-page WebRTC/media APIs are real (fake-device mic + MediaRecorder +
# AudioContext decode): the browser records, converts to WAV, and POSTs the
# audio over the WS RPC — it never holds the STT URL or key.
#
# Browser driver: playwright via npm (ephemeral install) with a system
# Chrome/Chromium binary. Missing node, a failed playwright install, or no
# usable Chromium FAILS the lane (exit 1, setup); assertion failures exit 2+.
#
# Usage: bash E2E.d/web/stt_rpc.sh [run|list]
set -euo pipefail
SCRIPT_DIR="$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
E2E_DIR="$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd -P)"
# shellcheck source=../lib/common.sh
. "$E2E_DIR/lib/common.sh"
# shellcheck source=lib/fixture.sh
. "$SCRIPT_DIR/lib/fixture.sh"

SCENARIO="web-stt-rpc"
TOKEN="web-stt-rpc-e2e-token-$$-$(date +%s)"
FIXTURE_KEY="stt-rpc-fixture-key"

run_phase() {
    local phase="$1" root="$2" evidence="$3" mock_error="$4"
    printf '%s\n' "$TOKEN" >"$root/token"

    # The mock must be up BEFORE the settings file is written: the live
    # sttBaseUrl points at the mock port. MOCK_STT_EVIDENCE makes the mock
    # persist metadata (authPresent/contentType/file/wav/model — never the
    # key or the audio body); MOCK_STT_ERROR=1 makes the endpoint reject
    # with 500 for the S5 error phase.
    local stt_evidence="$evidence/stt-proxy-evidence.json"
    if [ "$mock_error" = "1" ]; then
        MOCK_STT_EVIDENCE="$stt_evidence" MOCK_STT_ERROR=1 web_start_mock "$root" "$evidence"
    else
        MOCK_STT_EVIDENCE="$stt_evidence" web_start_mock "$root" "$evidence"
    fi
    local port
    port="$(cat "$root/mock-port.txt")"

    # Live hold-to-talk STT settings in the isolated agent dir so the Web
    # listener advertises runtimeSettings.live = {enabled: true, mode: "stt"}
    # and the composer mic enters hold-to-talk mode. sttApiKey is a fixture
    # placeholder written directly to the fixture settings file (never
    # logged); the browser never receives it.
    cat >"$root/home/.pi/agent/settings.json" <<EOF
{
  "live": {
    "enabled": true,
    "mode": "stt",
    "sttBaseUrl": "http://127.0.0.1:$port",
    "sttApiKey": "$FIXTURE_KEY",
    "sttModel": "whisper-1",
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
        "$SCRIPT_DIR/stt_rpc_test.mjs" \
        "RPI_PHASE=$phase" \
        "RPI_STT_EVIDENCE=$stt_evidence" \
        "RPI_FIXTURE_KEY=$FIXTURE_KEY" \
        "RPI_STT_BASE_URL=http://127.0.0.1:$port" || pw_status=$?
    if [ "$pw_status" -eq 0 ]; then
        web_sanity_http "$url" >/dev/null
    fi
    web_kill_rpi
    web_kill_mock "$root"
    if [ "$pw_status" -eq 0 ]; then
        printf 'playwright (%s): hold-to-talk STT RPC lifecycle PASSED\n' "$phase" | tee "$evidence/playwright-$phase-summary.txt"
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
                'web-stt-rpc - user-visible hold-to-talk STT against the REAL Rust proxy: synthetic-mic hold dispatches stt_transcribe ({audioBase64, mimeType} only, no URL/key), the mock /v1/audio/transcriptions records the server-held Bearer + multipart WAV + model (metadata-only evidence), the transcript lands in the composer, the fixture key never appears in DOM/evidence; mock-500 surfaces the bounded transcription-failed toast with no transcript (PLAYWRIGHT-ONLY, hard-fail)'
            return 0
            ;;
        run|all) ;;
        *) fail "usage: $0 [run|list]" ;;
    esac

    require_rpi
    web_require_browser

    # Phase 1: success lifecycle (hold -> RPC -> proxy evidence -> transcript).
    local root evidence
    root="$(scenario_workspace "$SCENARIO")"
    evidence="$EVIDENCE_ROOT/$SCENARIO"
    mkdir -p "$root/home/.pi/agent" "$root/workspace"
    run_phase "ok" "$root" "$evidence" "0"

    # Phase 2: error lifecycle (mock 500 -> bounded toast, no transcript).
    local root2 evidence2
    root2="$(scenario_workspace "$SCENARIO-error")"
    evidence2="$EVIDENCE_ROOT/$SCENARIO-error"
    mkdir -p "$root2/home/.pi/agent" "$root2/workspace"
    run_phase "error" "$root2" "$evidence2" "1"

    web_finish_lane "$SCENARIO"
}

main "$@"
