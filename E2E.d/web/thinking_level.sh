#!/usr/bin/env bash
# Web thinking-level persistence E2E lane (playwright-only hard gate): the
# real listener + loopback reasoning mock; #thinking-select must send exactly
# one authoritative set_thinking_level per selection, remain selected idle and
# while streaming, preserve per-session levels across switches, and restore
# from a reload bootstrap. Playwright: setup failure exits 1, assertions 2+.
set -euo pipefail
SCRIPT_DIR="$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
E2E_DIR="$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd -P)"
. "$E2E_DIR/lib/common.sh"
. "$SCRIPT_DIR/lib/fixture.sh"

SCENARIO="web-thinking-level"
TOKEN="web-thinking-level-e2e-token-$$-$(date +%s)"
SLOW_PREFIX="steer-1-"
FAST_REPLY="steering-followup-reply"
MOCK_REASONING=1

main() {
    case "${1:-run}" in
        list|--list|--dry-run)
            printf '%s\n' \
                'web-thinking-level - exactly one authoritative set_thinking_level per selection, stays selected (idle + streaming next-turn), per-session levels across switches, reload/bootstrap restores (playwright, hard gate)'
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

    export MOCK_REASONING
    port="$(web_start_mock "$root" "$evidence")"
    web_spawn_rpi "$root" "$evidence" "$port"
    url="$(web_wait_for_listener "$evidence")"
    log "$SCENARIO: listener at $url"

    local pw_status=0
    web_run_playwright "$url" "$evidence" "$root/playwright" "$SCRIPT_DIR/thinking_level_test.mjs" \
        RPI_SLOW_PREFIX="$SLOW_PREFIX" RPI_FAST_REPLY="$FAST_REPLY" || pw_status=$?
    if [ "$pw_status" -eq 0 ]; then
        printf 'playwright: thinking-level exactly-once + remains-selected + per-session + restore: PASSED\n' | tee "$evidence/playwright-summary.txt"
    elif [ "$pw_status" -eq 1 ]; then
        fail "$SCENARIO: playwright SETUP FAILED (node/chromium/npm unavailable)"
    else
        fail "$SCENARIO: playwright lane failed (exit $pw_status)"
    fi

    web_sanity_http "$url"
    web_finish_lane "$SCENARIO"
}

main "$@"