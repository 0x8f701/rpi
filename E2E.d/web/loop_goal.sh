#!/usr/bin/env bash
# Web composer /loop + /goal + /ps E2E lane (playwright-only hard gate).
#
# Spawns the real `rpi --listen` binary with a token file and the loopback
# mock provider (--scenario steering; no user prompts are typed, so the mock
# only serves the model catalog plus the incidental loop-fire and goal-work
# turns), opens the served `/web` page in a real browser, and asserts the Web
# composer command surface for the backend-builtin /loop, /goal, and /ps
# commands:
#   - opening the command picker lists /loop + /goal + /ps (get_commands
#     catalog authority) alongside the previously wired commands
#   - choosing /loop drafts "/loop " (requiresArguments -> trailing space) and
#     choosing /goal drafts "/goal" (bare) — neither auto-submits (no user
#     bubble, no RPC, no summary bubble); choosing /ps drafts "/ps" (bare)
#   - Enter dispatches the typed RPCs against the REAL listener:
#       /loop create <interval> <prompt> -> "scheduled <id> · …" bubble
#       /loop list                       -> task row bubble
#       /loop update <id> <interval> <p> -> "updated loop <id> · …" bubble
#       /loop delete <id>                -> "deleted loop <id>" bubble
#       /loop cancel <id> (after delete) -> actionable "no active loop" error
#       /goal create --tokens N <obj>    -> "Goal work started|queued · active
#                                            · 0/N tokens · <obj>" bubble
#       /goal show                       -> state line bubble
#       /goal pin <text> + /goal pins    -> numbered pins bubble
#       /goal pause / resume / complete  -> lifecycle bubbles
#       /goal drop (after complete)      -> actionable invalid-transition error
#       /ps (bare)                       -> process list bubble ("No supervised
#                                            processes" in the empty fixture)
#   - /goal create|resume dispatch `activate: true` (TUI parity: they start
#     or queue goal work) and render the activation prefix + chained
#     goal_get state line
#   - TUI parity streaming guard: /loop update while a turn is running is
#     rejected locally with the TUI-equivalent toast and dispatches NO RPC
#   - malformed arguments fail LOCALLY with TUI-equivalent usage toasts and
#     dispatch NO RPC: bare /loop (loop requires args), /goal unpin nope,
#     /goal pin (missing text), /ps extra (bare-only surface)
#   - intercepted commands never create an optimistic user bubble (the user
#     bubble count is unchanged across every dispatch)
#   - every loop_*/goal_*/process_list frame carries the active session's
#     top-level sessionId (no cross-session routing: the multi-session
#     contract)
#   - the lane writes $RPI_EVIDENCE/coverage-assertions.json for the Web
#     coverage matrix (feature "loop + goal composer")
#
# Browser driver: playwright via npm (ephemeral install in the scenario work
# dir) with a system Chrome/Chromium binary or playwright's bundled chromium.
# Missing node, a failed playwright install, or no usable Chromium FAILS the
# lane (exit 1, setup); assertion failures exit 2+.
#
# Usage: bash E2E.d/web/loop_goal.sh [run|list]
set -euo pipefail
SCRIPT_DIR="$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
E2E_DIR="$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd -P)"
# shellcheck source=../lib/common.sh
. "$E2E_DIR/lib/common.sh"
# shellcheck source=lib/fixture.sh
. "$SCRIPT_DIR/lib/fixture.sh"

SCENARIO="web-loop_goal"
TOKEN="web-loop-goal-e2e-token-$$-$(date +%s)"

main() {
    case "${1:-run}" in
        list|--list|--dry-run)
            printf '%s\n' \
                'web-loop-goal - composer picker lists /loop + /goal + /ps (get_commands authority), draft-no-auto-submit, Enter dispatches loop create/list/update/delete/cancel + goal create/show/pin/pins/pause/resume/complete/drop + bare /ps (empty process list) against the real listener, local TUI-equivalent usage toasts for malformed args (incl. /ps extra) with no RPC, no optimistic user bubbles, every frame sessionId-stamped (playwright, hard gate)'
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
        "$SCRIPT_DIR/loop_goal_test.mjs" || pw_status=$?
    if [ "$pw_status" -eq 0 ]; then
        printf 'playwright: /loop + /goal composer picker + real-listener RPC dispatch: PASSED\n' | tee "$evidence/playwright-summary.txt"
    elif [ "$pw_status" -eq 1 ]; then
        fail "$SCENARIO: playwright SETUP FAILED (node/chromium/npm unavailable)"
    else
        fail "$SCENARIO: playwright lane failed (exit $pw_status)"
    fi

    web_sanity_http "$url"
    web_finish_lane "$SCENARIO"
}

main "$@"
