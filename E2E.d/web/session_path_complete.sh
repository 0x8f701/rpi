#!/usr/bin/env bash
# Web session path-completion + authenticated catalog E2E lane
# (playwright-only hard gate).
#
# Spawns the real `rpi --listen` binary WITH a token file and the loopback
# mock provider (--scenario steering), opens `/web` in a real browser, and
# asserts the SessionPanel × PathAutocomplete contract against the REAL
# backend (the node-level regressions live in
# crates/pi-cli/web/scripts/sessionPanelAutocomplete.test.ts):
#   1. TOKEN AUTH: the tokened boot holds, the good token authenticates the
#      rpi-auth.<token> subprotocol, and the session_list catalog loads only
#      over the authenticated socket with exact row identity
#   2. SUCCESS: typing a valid existing workdir under the displayed session's
#      authoritative workspace root returns directory suggestions in the
#      bounded listbox with zero toasts, zero panel status, and no sidebar
#      error
#   3. MISSING IN-WORKSPACE + MANUAL FAILURE: a missing directory under the
#      workspace fails closed quietly — no suggestion menu, zero error
#      toasts, zero panel status, no sidebar error; manual Enter emits a
#      new_session failure with its owned toast
#
# Usage: bash E2E.d/web/session_path_complete.sh [run|list]
set -euo pipefail
SCRIPT_DIR="$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
E2E_DIR="$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd -P)"
# shellcheck source=../lib/common.sh
. "$E2E_DIR/lib/common.sh"
# shellcheck source=lib/fixture.sh
. "$SCRIPT_DIR/lib/fixture.sh"

SCENARIO="web-session-path-complete"
TOKEN="web-path-complete-$$-$(date +%s)"

main() {
    case "${1:-run}" in
        list|--list|--dry-run)
            printf '%s\n' \
                'web-session-path-complete - tokened catalog load with exact row identity, valid-workdir completion suggestions, missing-workdir fail-closed quiet + manual new_session failure (playwright, hard gate)'
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
    mkdir -p "$root/workspace/existing-dir"
    printf 'completion fixture\n' >"$root/workspace/existing-dir/keep.txt"
    if [ -e "$root/workspace/absent-e2e-dir" ]; then
        fail "$SCENARIO: absent fixture directory must not exist before the lane"
    fi

    port="$(web_start_mock "$root" "$evidence")"
    web_spawn_rpi "$root" "$evidence" "$port"
    url="$(web_wait_for_listener "$evidence")"
    log "$SCENARIO: listener at $url"

    local pw_status=0
    web_run_playwright "$url" "$evidence" "$root/playwright" "$SCRIPT_DIR/session_path_complete_test.mjs" \
        RPI_TOKEN="$TOKEN" \
        RPI_COMPLETION_DIR="$root/workspace/existing-dir" \
        RPI_WORKSPACE="$root/workspace" || pw_status=$?
    if [ "$pw_status" -eq 0 ]; then
        printf 'playwright: token-authenticated catalog + valid-workdir suggestions + missing-workdir fail-closed + manual new_session failure: PASSED\n' | tee "$evidence/playwright-summary.txt"
    elif [ "$pw_status" -eq 1 ]; then
        fail "$SCENARIO: playwright SETUP FAILED (node/chromium/npm unavailable)"
    else
        fail "$SCENARIO: playwright lane failed (exit $pw_status)"
    fi

    web_sanity_http "$url"
    web_finish_lane "$SCENARIO"
}

main "$@"