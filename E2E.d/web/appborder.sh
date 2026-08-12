#!/usr/bin/env bash
# Web app-main panel border regression lane (playwright-only hard gate).
#
# Spawns the real `rpi --listen` binary with a token file and the loopback
# mock provider, opens `/web` in a real browser, and asserts the desktop
# dark/light panel-border contract + the mobile no-overflow contract:
#   - desktop (.app-main computed border-left/right: 1px solid the theme's
#     --border-strong token; dark #3a4350, light #8f959e) — the single strong
#     edge that visually separates the transcript column from the sidebar;
#   - header border-bottom and .app-main > footer border-top are the same
#     1px solid --border-strong edge;
#   - .session-sidebar carries NO border-right on desktop (the edge is owned
#     by .app-main's border-left — no doubled 2px seam);
#   - #transcript background resolves to --bg and scrollbar-gutter is stable;
#   - collapsing the rail via #sidebar-toggle-btn keeps the .app-main edge;
#   - mobile (390x844): .app-main drops BOTH edges (border-left/right-style
#     none) and the document never overflows horizontally
#     (documentElement.scrollWidth <= clientWidth, tolerance 0).
#
# The color assertion resolves the --border-strong token through a probe
# element (never a hardcoded hex), so the lane tracks the theme tokens.
#
# Browser driver: playwright via npm (ephemeral install) with a system
# Chrome/Chromium binary or playwright's bundled chromium. Missing node, a
# failed playwright install, or no usable Chromium FAILS the lane (exit 1,
# setup); assertion failures exit 2+.
#
# Usage: bash E2E.d/web/appborder.sh [run|list]
set -euo pipefail
SCRIPT_DIR="$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
E2E_DIR="$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd -P)"
# shellcheck source=../lib/common.sh
. "$E2E_DIR/lib/common.sh"
# shellcheck source=lib/fixture.sh
. "$SCRIPT_DIR/lib/fixture.sh"

SCENARIO="web-appborder"
TOKEN="web-appborder-e2e-token-$$-$(date +%s)"

# ---------------------------------------------------------------------------
main() {
    case "${1:-run}" in
        list|--list|--dry-run)
            printf '%s\n' \
                'web-appborder - desktop dark/light .app-main computed border (1px solid --border-strong both edges, header/footer edges, no rail seam, stable transcript surface) + mobile (390x844) flush edges with zero horizontal overflow (PLAYWRIGHT-ONLY, hard-fail)'
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
    web_run_playwright "$url" "$evidence" "$root/playwright" \
        "$SCRIPT_DIR/appborder_test.mjs" || pw_status=$?
    if [ "$pw_status" -eq 0 ]; then
        printf 'playwright: app-main border + mobile overflow contract PASSED\n' | tee "$evidence/playwright-summary.txt"
    elif [ "$pw_status" -eq 1 ]; then
        fail "$SCENARIO: playwright SETUP FAILED (node/chromium/npm unavailable)"
    else
        fail "$SCENARIO: playwright lane failed (exit $pw_status)"
    fi

    web_sanity_http "$url"
    web_finish_lane "$SCENARIO"
}

main "$@"
