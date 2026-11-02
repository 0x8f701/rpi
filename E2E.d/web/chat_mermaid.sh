#!/usr/bin/env bash
# Web /chat SessionView mermaid scroll+geometry E2E lane (playwright-only
# hard gate). Reproduces the user-reported defect surface end-to-end: the
# /chat SessionView transcript (chat/SessionView.tsx + shared markdown.ts)
# against a real `rpi --listen` runner and the loopback steering mock.
#
# Spawns the real rpi --listen binary with a token file and the steering
# mock ("render rich content" -> narrow mermaid flowchart + KaTeX,
# "render wide mermaid" -> a long-labeled LR chain far wider than any
# viewport), opens `/chat` in a real browser, adds the listener as a
# runner, maps a project session, and asserts on the REAL SessionView:
#   Desktop 1360x900:
#     1. a rendered .md-mermaid-host svg appears in .chat-session__transcript
#     2. PIN ON ASYNC HYDRATION (the user defect): the transcript viewport
#        stays glued to the bottom across the async mermaid layout growth —
#        remaining-to-bottom stays within the pin tolerance after hydration
#        settles, with NO manual scrolling
#     3. NARROW CENTER GEOMETRY: the narrow diagram is NOT classified wide
#        (no data-mermaid-wide, no min-width) and is horizontally centered
#        by the shared margin-inline:auto (equal left/right gaps)
#     4. USER-UPSCROLL NEVER STOLEN: after a real mouse-wheel scroll away
#        from the bottom, a new prompt + streamed reply does not move the
#        viewport (scrollTop frozen while content grows below)
#     5. WIDE FIT: the wide diagram is classified wide
#        (data-mermaid-wide + intrinsic min-width) and /chat SessionView
#        scales it inside the cap (no horizontal scrollbar)
#   Mobile 390x844:
#     6. the same narrow center geometry on a fresh mobile page
#   Zero console errors / page errors; desktop + mobile screenshots captured.
#
# Usage: bash E2E.d/web/chat_mermaid.sh [run|list]
set -euo pipefail
SCRIPT_DIR="$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
E2E_DIR="$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd -P)"
# shellcheck source=../lib/common.sh
. "$E2E_DIR/lib/common.sh"
# shellcheck source=lib/fixture.sh
. "$SCRIPT_DIR/lib/fixture.sh"

SCENARIO="web-chat-mermaid"
TOKEN="web-chat-mermaid-e2e-token-$$-$(date +%s)"

# ---------------------------------------------------------------------------
main() {
    case "${1:-run}" in
        list|--list|--dry-run)
            printf '%s\n' \
                'web-chat-mermaid - real-browser /chat SessionView mermaid: async-hydration bottom pin, narrow center geometry, user-upscroll never stolen, wide horizontal scroll, mobile 390x844 (playwright, hard gate)'
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
    web_run_playwright "$url" "$evidence" "$root/playwright" "$SCRIPT_DIR/chat_mermaid_test.mjs" \
        "RPI_CHAT_URL=${url%/web}/chat" \
        "RPI_RUNNER_ORIGIN=${url%/web}" \
        "RPI_RUNNER_TOKEN=$TOKEN" \
        "RPI_RUNNER_CWD=$root/workspace" || pw_status=$?
    if [ "$pw_status" -eq 0 ]; then
        printf 'playwright: /chat SessionView mermaid pin+geometry desktop+mobile: PASSED\n' | tee "$evidence/playwright-summary.txt"
    elif [ "$pw_status" -eq 1 ]; then
        fail "$SCENARIO: playwright SETUP FAILED (node/chromium/npm unavailable)"
    else
        fail "$SCENARIO: playwright lane failed (exit $pw_status)"
    fi

    web_sanity_http "$url"
    web_finish_lane "$SCENARIO"
}

main "$@"