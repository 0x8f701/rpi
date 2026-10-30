#!/usr/bin/env bash
# Web collapsed-rail reopen hit-target lane (playwright-only hard gate).
#
# Spawns the real `rpi --listen` binary with a token file and the loopback
# mock provider, opens `/web` in a real browser at desktop >720px, and asserts:
#   - real click on #sidebar-collapse-btn folds the rail to 44px with a
#     visible #rail-reopen-btn (no force/dispatch);
#   - real click on #rail-reopen-btn restores app-layout--drawer-open and
#     sidebar content;
#   - collapse/reopen cycle repeats;
#   - keyboard Enter and Space on the focused reopen button restore the rail;
#   - narrow mobile (<720px) drawer behavior is unaffected;
#   - screenshots per phase and zero console/page errors.
#
# Pre-fix CSS fails this lane: `.session-sidebar *:not(#rail-reopen-btn)` sets
# pointer-events:none on `.session-sidebar__head`, so locator.click on the
# visible » times out / never sets sidebarOpen=true.
#
# Browser driver: playwright via npm (ephemeral install) with a system
# Chrome/Chromium binary or playwright's bundled chromium. Missing node, a
# failed playwright install, or no usable Chromium FAILS the lane (exit 1,
# setup); assertion failures exit 2+.
#
# Usage: bash E2E.d/web/sidebar_rail.sh [run|list]
set -euo pipefail
SCRIPT_DIR="$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
E2E_DIR="$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd -P)"
# shellcheck source=../lib/common.sh
. "$E2E_DIR/lib/common.sh"
# shellcheck source=lib/fixture.sh
. "$SCRIPT_DIR/lib/fixture.sh"

SCENARIO="web-sidebar-rail"
TOKEN="web-sidebar-rail-e2e-token-$$-$(date +%s)"

# ---------------------------------------------------------------------------
main() {
    case "${1:-run}" in
        list|--list|--dry-run)
            printf '%s\n' \
                'web-sidebar-rail - desktop collapsed-rail #rail-reopen-btn is a real hit target (click + Enter/Space) restoring the drawer; mobile <720px unaffected (PLAYWRIGHT-ONLY, hard-fail)'
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
        "$SCRIPT_DIR/sidebar_rail_test.mjs" || pw_status=$?
    if [ "$pw_status" -eq 0 ]; then
        printf 'playwright: collapsed-rail reopen hit-target PASSED\n' | tee "$evidence/playwright-summary.txt"
    elif [ "$pw_status" -eq 1 ]; then
        fail "$SCENARIO: playwright SETUP FAILED (node/chromium/npm unavailable)"
    else
        fail "$SCENARIO: playwright lane failed (exit $pw_status)"
    fi

    web_sanity_http "$url"
    web_finish_lane "$SCENARIO"
}

main "$@"
