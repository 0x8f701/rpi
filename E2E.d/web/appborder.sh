#!/usr/bin/env bash
# Web app-main panel border + shared drawer resize + sidebar collapse lane
# (playwright-only hard gate).
#
# Spawns the real `rpi --listen` binary with a token file and the loopback
# mock provider, opens `/web` in a real browser, and asserts:
#   - desktop (.app-main computed border-left/right: 1px solid the theme's
#     --border-strong token; dark #3a4350, light #8f959e);
#   - header border-bottom and .app-main > footer border-top match;
#   - .session-sidebar has NO border-right on desktop;
#   - #transcript background resolves to --bg and scrollbar-gutter is stable;
#   - rail collapse via #sidebar-toggle-btn keeps the .app-main edge;
#   - ordinary panels share ONE desktop height resizer (#panel-drawer-resizer):
#     pointer + keyboard + 25–90vh bounds + localStorage `rpi-panel-drawer-size`;
#   - SessionSidebar header #sidebar-collapse-btn folds desktop rail / closes
#     mobile drawer; #rail-reopen-btn + header ☰ remain;
#   - mobile (390x844): flush edges, zero horizontal overflow, resizer hidden.
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
                'web-appborder - desktop dark/light .app-main edges + shared panel resizer (pointer/keyboard/bounds/reload) + sidebar header collapse/reopen + mobile flush edges / no resizer / zero overflow (PLAYWRIGHT-ONLY, hard-fail)'
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
