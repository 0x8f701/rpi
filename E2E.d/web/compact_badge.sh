#!/usr/bin/env bash
# Web auto-compaction badge lane (PLAYWRIGHT-ONLY hard gate).
#
# Reported bug: after a REAL post-turn threshold compaction the header
# `#compact-badge` kept pulsing forever — the page never received a false
# `get_state` snapshot, so a badge that only clears on a later state poll
# stays visible with the authority already idle.
#
# Fixture: the shared web fixture (`lib/fixture.sh`) — real `rpi --listen`
# binary with a token file + loopback mock provider — but the mock is the repo
# `sessions` scenario wrapped by `lib/compact_gate_mock.py`, which holds ONLY
# the summarizer request. A real page prompt (echoed back at ~5.8k chars, so
# the mock reports >4000 usage tokens against the 4000-token window) drives
# the app's own post-turn threshold compaction; the page's own 5s stream
# watchdog then issues the automatic `get_state` that paints the badge. No
# signals and no UI mutation.
#
# Then: real `compaction_end` (reason=threshold, ordered after start), an
# authoritative `isCompacting=false` from a separate RPC socket, and the badge
# MUST clear within 3000ms of the page-clock end receipt, with NO false
# `get_state` snapshot before that hide (so the clear comes from the end
# event, never a later authority poll).
# Also asserts the badge paints the theme's --accent token during the
# compaction in dark AND light, that prefers-reduced-motion drops the pulse,
# and that 390x844 stays overflow-free (in flight and after the end).
#
# Usage: bash E2E.d/web/compact_badge.sh [run|list]
set -euo pipefail
SCRIPT_DIR="$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
E2E_DIR="$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd -P)"
# shellcheck source=../lib/common.sh
. "$E2E_DIR/lib/common.sh"
# shellcheck source=lib/fixture.sh
. "$SCRIPT_DIR/lib/fixture.sh"

SCENARIO="web-compact-badge"
TOKEN="web-compact-badge-e2e-token-$$-$(date +%s)"
GATE_MOCK="$SCRIPT_DIR/lib/compact_gate_mock.py"

main() {
    case "${1:-run}" in
        list|--list|--dry-run)
            printf '%s\n' \
                'web-compact-badge - real auto threshold compaction: page watchdog state paints #compact-badge, real compaction_end + authority idle hides it promptly with zero false state snapshot before the hide; --accent in dark+light, reduced-motion, 390x844 in flight and after end (PLAYWRIGHT-ONLY, hard-fail)'
            return 0
            ;;
        run|all) ;;
        *) fail "usage: $0 [run|list]" ;;
    esac

    require_rpi
    web_require_browser
    require_cmd python3

    local root evidence port url gate_release gate_hit mock_pid mock_start pw_status=0
    prepare_roots
    root="$(scenario_workspace "$SCENARIO")"
    trap 'web_kill_mock "$root"; cleanup_e2e' EXIT HUP INT TERM
    evidence="$EVIDENCE_ROOT/$SCENARIO"
    printf '%s\n' "$TOKEN" >"$root/token"

    # Tiny context window + a gated summarizer: the app's own post-turn
    # threshold compaction fires on one real prompt.
    cat >"$root/home/.rpi/settings.json" <<'EOF'
{"compaction":{"enabled":true,"reserveTokens":16,"keepRecentTokens":1,"snapKeepTurns":10}}
EOF

    gate_release="$root/gate-release"
    gate_hit="$root/gate-hit"
    RPI_REPO_ROOT="$REPO_ROOT" \
    RPI_GATE_RELEASE="$gate_release" \
    RPI_GATE_HIT="$gate_hit" \
    RPI_GATE_PORT_FILE="$root/mock-port.txt" \
        python3 "$GATE_MOCK" >"$evidence/mock-server.log" 2>&1 &
    printf '%s %s\n' "$!" "$(pid_start_token "$!")" >"$root/mock.pid"
    register_pid "$!"
    local deadline=$((SECONDS + 15))
    while [ ! -s "$root/mock-port.txt" ] && [ "$SECONDS" -lt "$deadline" ]; do sleep 0.1; done
    [ -s "$root/mock-port.txt" ] || fail "$SCENARIO: gated mock did not write its port file"
    port="$(cat "$root/mock-port.txt")"
    read -r mock_pid mock_start <"$root/mock.pid"
    [ -n "$mock_pid" ] && [ -n "$mock_start" ] || fail "$SCENARIO: mock identity was not recorded"

    # The 4000-token window is what makes the ~5.8k-char echo cross the
    # threshold.
    cat >"$root/home/.rpi/models.json" <<EOF
{
  "providers": {
    "user-steering": {
      "baseUrl": "http://127.0.0.1:$port",
      "api": "openai-completions",
      "models": [
        { "id": "mock", "name": "Steering Mock", "contextWindow": 4000, "maxTokens": 2048 }
      ]
    }
  }
}
EOF

    web_spawn_rpi "$root" "$evidence" "$port"
    url="$(web_wait_for_listener "$evidence")"
    log "$SCENARIO: listener at $url (gated mock pid $mock_pid)"

    web_run_playwright "$url" "$evidence" "$root/playwright" "$SCRIPT_DIR/compact_badge_test.mjs" \
        RPI_GATE_RELEASE="$gate_release" || pw_status=$?
    if [ "$pw_status" -ne 0 ]; then
        # The mock is lane-owned and stopped explicitly before the trap is
        # handed back to cleanup_e2e: the `$root`-referencing trap below only
        # owns the window while `root` (a main() local) is still in scope, so
        # it must never be the EXIT handler of record (a post-return EXIT would
        # read the expired local and abort under `set -u`).
        web_kill_mock "$root"
        trap cleanup_e2e EXIT HUP INT TERM
        if [ "$pw_status" -eq 1 ]; then
            fail "$SCENARIO: playwright SETUP FAILED (node/chromium/npm unavailable)"
        fi
        fail "$SCENARIO: playwright lane failed (exit $pw_status)"
    fi
    if [ ! -s "$gate_hit" ]; then
        web_kill_mock "$root"
        trap cleanup_e2e EXIT HUP INT TERM
        fail "$SCENARIO: summarizer gate was never hit — threshold compaction did not reach the provider"
    fi
    printf 'playwright: auto-compaction badge clears on compaction_end without a later state poll PASSED\n' | tee "$evidence/playwright-summary.txt"

    web_sanity_http "$url"
    web_kill_mock "$root"
    trap cleanup_e2e EXIT HUP INT TERM
    web_finish_lane "$SCENARIO"
}

main "$@"
