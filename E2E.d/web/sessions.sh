#!/usr/bin/env bash
# Web multi-session E2E lane — PLAYWRIGHT-ONLY (no agent-browser fallback, no
# skip). Requires the MultiSessionRuntimeManager backend (top-level sessionId
# routing, lifecycle {sessionId,state,messages} snapshots, per-session events,
# MAX_LOADED_SESSIONS=8 cap, close_session idle/busy semantics).
#
# Spawns the real `rpi --listen` binary with a token file and the loopback
# mock provider (--scenario sessions; content-routed replies so concurrently
# running session runtimes stay deterministic), opens `/web` in a real
# playwright browser, and asserts (see sessions_test.mjs for the T0.x–T7.x
# assertion matrix):
#   T1  slow session A keeps streaming while B is active and B's prompt
#       round-trips; background unread; unread clears on switch-back;
#       authoritative transcript restore
#   T2  abort + toast isolation (aborting B never affects A's stream)
#   T3  close_session busy refusal surfaced, then idle close succeeds
#       (loaded marker drops)
#   T4  8-session cap, no eviction, error surfaced
#   T5  Todo/Goal/Workflow state never leaks across sessions
#   T6  desktop rail collapse/reopen, sidebar New/Manage/switch, header has
#       NO feature buttons
#   T7  Android 390x844 drawer: hamburger opens, session pick closes the
#       drawer and restores the picked session's transcript
#
# The lane FAILS (non-zero) whenever playwright/chromium cannot be used —
# RPI_WEB_FORCE_PLAYWRIGHT is irrelevant here because there is no fallback.
#
# Usage: bash E2E.d/web/sessions.sh [run|list]
set -euo pipefail
SCRIPT_DIR="$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
E2E_DIR="$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd -P)"
# shellcheck source=../lib/common.sh
. "$E2E_DIR/lib/common.sh"
# shellcheck source=lib/fixture.sh
. "$SCRIPT_DIR/lib/fixture.sh"

SCENARIO="web-sessions"
TOKEN="web-sessions-e2e-token-$$-$(date +%s)"

# ---------------------------------------------------------------------------
main() {
    case "${1:-run}" in
        list|--list|--dry-run)
            printf '%s\n' \
                'web-sessions - multi-session: concurrent streaming, source-session routing, background unread, abort/toast isolation, close busy refusal + idle success, 8-session cap, Todo/Goal/Workflow isolation, desktop collapse rail, mobile drawer pick-close (PLAYWRIGHT-ONLY, hard-fail)'
            return 0
            ;;
        run|all) ;;
        *) fail "usage: $0 [run|list]" ;;
    esac

    require_rpi
    # Playwright-only: node is REQUIRED. Missing node is a FAILURE, never a
    # skip and never a fallback to agent-browser.
    require_cmd node

    local root evidence port url pw_status=0
    root="$(scenario_workspace "$SCENARIO")"
    evidence="$EVIDENCE_ROOT/$SCENARIO"
    printf '%s\n' "$TOKEN" >"$root/token"

    port="$(MOCK_SCENARIO=sessions web_start_mock "$root" "$evidence")"
    web_spawn_rpi "$root" "$evidence" "$port"
    url="$(web_wait_for_listener "$evidence")"
    log "$SCENARIO: listener at $url"

    # Hard playwright run: exit 1 from web_run_playwright means the npm
    # install failed — for THIS lane that is a real failure (no fallback).
    web_run_playwright "$url" "$evidence" "$root/playwright" "$SCRIPT_DIR/sessions_test.mjs" \
        "RPI_MOCK_CONTROL_URL=http://127.0.0.1:$port" || pw_status=$?
    if [ "$pw_status" -ne 0 ]; then
        fail "$SCENARIO: playwright lane failed (exit $pw_status) — sessions lane is playwright-only"
    fi

    web_sanity_http "$url"
    web_finish_lane "$SCENARIO"
}

main "$@"
