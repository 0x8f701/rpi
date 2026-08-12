#!/usr/bin/env bash
# Web transcript scroll-pinning E2E lane (playwright-only hard gate).
#
# Spawns the real `rpi --listen` binary with a token file and the loopback
# mock provider (--scenario scroll), opens /web in a real browser, and
# asserts the streaming-transcript bottom-pin contract with a per-frame
# in-page sampler (see scroll_test.mjs for the full assertion matrix):
#   1. bootstrap + long stream ("scroll-long-a") stay pinned: EVERY frame
#      sample keeps the remaining distance within tolerance; each direct
#      DOM text-delta advances scrollTop synchronously; window/document
#      scroll metrics stay at 0 (no window/body double scroll, no browser
#      scroll-anchoring interference).
#   2. a manual scroll away unpins: incoming deltas preserve the viewport
#      EXACTLY (scrollTop frozen on every frame) while the transcript
#      keeps growing.
#   3. scrolling back to the bottom re-pins: the next deltas stay glued.
#   4. streaming -> final rendered markdown ("scroll-final-md"): a tall
#      code fence (react commit), a Mermaid diagram (async SVG hydration)
#      and a data-URL image (async decode) must all keep the view glued —
#      the OLD nearBottom/useEffect logic has no ResizeObserver and drifts
#      here, so this is a deterministic old-logic discriminator.
#   5. narrow/mobile viewport ("scroll-narrow", 480px): header badge /
#      Abort button toggles at turn_start/turn_end plus composer reflow
#      must not break the pin while pinned or move the viewport while
#      unpinned (clientHeight changes absorbed by the container observer).
#   6. switching back to a long, previously unpinned session pins ITS
#      transcript to the bottom intentionally — the activated session
#      never inherits the previous session's scroll position or pin state
#      (forcePin; the old logic's stale nearBottom=false fails here).
#
# Evidence: screenshots per phase plus scroll-metrics.json (per-phase
# summaries + the full trace with classified growth events). On failure the
# first jump's before/after metrics and triggering event are reported and
# the trace is still written for diagnosis.
#
# Browser driver: playwright via npm (ephemeral install in the scenario work
# dir) with a system Chrome/Chromium binary or playwright's bundled chromium.
# Missing node, a failed playwright install, or no usable Chromium FAILS the
# lane (exit 1, setup); assertion failures exit 2+.
#
# Usage: bash E2E.d/web/scroll.sh [run|list]
set -euo pipefail
SCRIPT_DIR="$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
E2E_DIR="$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd -P)"
# shellcheck source=../lib/common.sh
. "$E2E_DIR/lib/common.sh"
# shellcheck source=lib/fixture.sh
. "$SCRIPT_DIR/lib/fixture.sh"

SCENARIO="web-scroll"
MOCK_SCENARIO="scroll"
TOKEN="web-scroll-e2e-token-$$-$(date +%s)"

# ---------------------------------------------------------------------------
main() {
    case "${1:-run}" in
        list|--list|--dry-run)
            printf '%s\n' \
                'web-scroll - streaming transcript bottom-pin contract: per-frame bounded pinned stream (delta sync, no anchoring), exact unpinned freeze, re-pin on return, async final-markdown/mermaid/image pin, narrow-layout pin, session switch force-pins the activated transcript (playwright, hard gate)'
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
    web_run_playwright "$url" "$evidence" "$root/playwright" "$SCRIPT_DIR/scroll_test.mjs" || pw_status=$?
    if [ "$pw_status" -eq 0 ]; then
        printf 'playwright: transcript scroll pinning contract: PASSED\n' | tee "$evidence/playwright-summary.txt"
    elif [ "$pw_status" -eq 1 ]; then
        fail "$SCENARIO: playwright SETUP FAILED (node/chromium/npm unavailable)"
    else
        fail "$SCENARIO: playwright lane failed (exit $pw_status)"
    fi

    web_sanity_http "$url"
    web_finish_lane "$SCENARIO"
}

main "$@"
