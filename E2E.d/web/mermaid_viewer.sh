#!/usr/bin/env bash
# Web Mermaid viewer E2E lane (playwright-only hard gate).
#
# Spawns the real `rpi --listen` binary with a token file and the loopback
# mock provider (--scenario steering; the "render rich content" prompt
# returns a valid mermaid flowchart, the "render markdown extra branches"
# prompt returns an invalid mermaid sequenceDiagram), builds the
# current-tree Web bundle, opens `/web` in a real browser, and asserts:
#   Desktop 1280x800:
#     1. every rendered .md-mermaid-host has the always-visible toolbar row
#        (View / Download SVG)
#     2. View opens ONE aria-modal dialog (lazy singleton); dblclick on the
#        rendered SVG opens the SAME singleton
#     3. Fit is the default transform; −/+/Fit/100% and wheel zoom + drag pan
#        update the stage transform
#     4. Escape/Close close the viewer and restore focus to the opener
#     5. SVG download: filename mermaid-diagram.svg, blob MIME
#        image/svg+xml, sanitized content (no script/foreignObject/embed,
#        no on* handlers, no non-local URL attributes)
#     6. the error diagram host has no controls and never opens the viewer
#   Mobile 390x844 (touch):
#     7. viewer controls are >=44px touch targets
#     8. the explicit View tap (NOT a dbltap) opens the viewer, the viewer
#        fits the viewport with no page overflow, and single-finger pan works
#   Zero console errors / page errors across both pages; inline + viewer
#   screenshots captured for desktop and mobile.
#
# Browser driver: playwright via npm (ephemeral install in the scenario work
# dir) with a system Chrome/Chromium binary or playwright's bundled chromium.
# Missing node, a failed playwright install, or no usable Chromium FAILS the
# lane (exit 1, setup); assertion failures exit 2+.
#
# Usage: bash E2E.d/web/mermaid_viewer.sh [run|list]
set -euo pipefail
SCRIPT_DIR="$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
E2E_DIR="$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd -P)"
# shellcheck source=../lib/common.sh
. "$E2E_DIR/lib/common.sh"
# shellcheck source=lib/fixture.sh
. "$SCRIPT_DIR/lib/fixture.sh"

SCENARIO="web-mermaid-viewer"
TOKEN="web-mermaid-viewer-e2e-token-$$-$(date +%s)"

# ---------------------------------------------------------------------------
main() {
    case "${1:-run}" in
        list|--list|--dry-run)
            printf '%s\n' \
                'web-mermaid-viewer - real-browser mermaid viewer: host toolbar, one aria-modal singleton (View/dblclick), Fit/zoom/wheel/drag, Escape/Close focus restore, sanitized SVG download, mobile 390px 44px controls + View tap + fit + pan, error host has no controls (playwright, hard gate)'
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
    web_run_playwright "$url" "$evidence" "$root/playwright" "$SCRIPT_DIR/mermaid_viewer_test.mjs" || pw_status=$?
    if [ "$pw_status" -eq 0 ]; then
        printf 'playwright: mermaid viewer desktop+mobile: PASSED\n' | tee "$evidence/playwright-summary.txt"
    elif [ "$pw_status" -eq 1 ]; then
        fail "$SCENARIO: playwright SETUP FAILED (node/chromium/npm unavailable)"
    else
        fail "$SCENARIO: playwright lane failed (exit $pw_status)"
    fi

    web_sanity_http "$url"
    web_finish_lane "$SCENARIO"
}

main "$@"