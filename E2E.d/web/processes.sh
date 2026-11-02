#!/usr/bin/env bash
# Web Processes panel E2E lane (playwright-only hard gate).
#
# Opens the Processes drawer from the sidebar, asserts the empty list, spawns
# a supervised bash -c command through #processes-spawn-btn (process_spawn),
# sees the labeled row, stops it (process_stop), then repeats the open on
# mobile 390x844. Missing node/chromium FAILS (exit 1).
#
# Usage: bash E2E.d/web/processes.sh [run|list]
set -euo pipefail
SCRIPT_DIR="$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
E2E_DIR="$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd -P)"
# shellcheck source=../lib/common.sh
. "$E2E_DIR/lib/common.sh"
# shellcheck source=lib/fixture.sh
. "$SCRIPT_DIR/lib/fixture.sh"

SCENARIO="web-processes"
TOKEN="web-processes-e2e-token-$$-$(date +%s)"

main() {
    case "${1:-run}" in
        list|--list|--dry-run)
            printf '%s\n' \
                'web-processes - Processes panel spawn/list/stop via #processes-panel; process_spawn + process_stop on the wire; desktop + mobile 390x844 (PLAYWRIGHT-ONLY, hard-fail)'
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
    web_run_playwright "$url" "$evidence" "$root/playwright" "$SCRIPT_DIR/processes_test.mjs" || pw_status=$?
    if [ "$pw_status" -eq 0 ]; then
        printf 'playwright: processes panel spawn/list/stop PASSED\n' | tee "$evidence/playwright-summary.txt"
    elif [ "$pw_status" -eq 1 ]; then
        fail "$SCENARIO: playwright SETUP FAILED (node/chromium/npm unavailable)"
    else
        fail "$SCENARIO: playwright lane failed (exit $pw_status)"
    fi

    web_sanity_http "$url"
    web_finish_lane "$SCENARIO"
}

main "$@"
