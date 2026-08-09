#!/usr/bin/env bash
# Web side-chat + maintenance E2E lane (playwright-only hard gate).
#
# Spawns the real `rpi --listen` binary with a token file and the loopback
# mock provider (--scenario steering; the side-chat turn is request 1, the
# slow stream), opens `/web` in a real browser, and asserts:
#   Side chat (multi-tab, D94 scope):
#     1. panel opens with the default tab; a new tab is created from the form
#     2. prompting the tab round-trips through the real side-chat session and
#        the assistant entry renders the streamed mock reply
#   Maintenance (compact A→B / rewind / handoff / queue):
#     3. Snapcompact renders the A→B token report ("N → M estimated tokens")
#     4. Rewind lists session records; Handoff renders the envelope
#     5. the queue view renders and Cancel queue reports the drain
#
# Browser driver: playwright via npm (ephemeral install in the scenario work
# dir) with a system Chrome/Chromium binary or playwright's bundled chromium.
# Missing node, a failed playwright install, or no usable Chromium FAILS the
# lane (exit 1, setup); assertion failures exit 2+.
#
# Usage: bash E2E.d/web/extras.sh [run|list]
set -euo pipefail
SCRIPT_DIR="$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
E2E_DIR="$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd -P)"
# shellcheck source=../lib/common.sh
. "$E2E_DIR/lib/common.sh"
# shellcheck source=lib/fixture.sh
. "$SCRIPT_DIR/lib/fixture.sh"

SCENARIO="web-extras"
TOKEN="web-extras-e2e-token-$$-$(date +%s)"
SLOW_TAIL="chunk-four-done"      # tail of the slow mock stream (side-chat turn)

# ---------------------------------------------------------------------------
main() {
    case "${1:-run}" in
        list|--list|--dry-run)
            printf '%s\n' \
                'web-extras - side chat multi-tab (new tab + prompt round-trip) + maintenance (snapcompact A→B report, rewind, handoff, queue view/cancel) (playwright, hard gate)'
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
    # snapcompact archives all but the last `snapKeepTurns` turns; default 10
    # would never be reachable in a short lane, so the fixture lowers it to 1
    # and the lane primes the session with two turns.
    printf '{"compaction": {"snapKeepTurns": 1}}\n' >"$root/home/.pi/agent/settings.json"
    web_spawn_rpi "$root" "$evidence" "$port"
    url="$(web_wait_for_listener "$evidence")"
    log "$SCENARIO: listener at $url"

    local pw_status=0
    web_run_playwright "$url" "$evidence" "$root/playwright" "$SCRIPT_DIR/extras_test.mjs" \
        RPI_SLOW_TAIL="$SLOW_TAIL" || pw_status=$?
    if [ "$pw_status" -eq 0 ]; then
        printf 'playwright: side chat multi-tab + maintenance: PASSED\n' | tee "$evidence/playwright-summary.txt"
    elif [ "$pw_status" -eq 1 ]; then
        fail "$SCENARIO: playwright SETUP FAILED (node/chromium/npm unavailable)"
    else
        fail "$SCENARIO: playwright lane failed (exit $pw_status)"
    fi

    web_sanity_http "$url"
    web_finish_lane "$SCENARIO"
}

main "$@"
