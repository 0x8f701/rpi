#!/usr/bin/env bash
# Web WS delayed-open/reconnect sidebar+panel regression lane
# (playwright-only hard gate).
#
# Spawns the real `rpi --listen` binary with a token file and the loopback
# mock provider (--scenario sessions: a primary session row is listed at
# boot), opens `/web` in a real browser, and asserts the ReadyGate contract
# for background catalog loads across a REAL server outage:
#   1. boot: WS reaches `on` and the sidebar lists the primary session row
#   2. the lane KILLS the listener mid-session -> the client enters
#      `reconnecting`; while the WS is CONNECTING (backoff) the sidebar's
#      8s poll and the session panel's load must NOT render any sidebar
#      error (the mount/poll loads wait on the ReadyGate for the socket to
#      reopen instead of failing fast): `.session-sidebar__error` count
#      stays zero, no sidebar error string appears in the body (minus
#      #toasts), and the retained rows stay listed
#   3. the lane respawns the listener on the SAME port -> the client
#      auto-reconnects to `on` and the sidebar + session panel eventually
#      load their real rows WITHOUT any user action (gated poll resolves on
#      notifyOpen)
#   4. the error concept is deleted: zero `.session-sidebar__error` nodes
#      and none of the sidebar error strings (`load failed`, `session_list
#      failed`, RPC-limit text, `workflow_detail`, `session switch
#      pending`) — sampled during the outage window (crossing at least one
#      8s sidebar poll), after the respawn, and after the panel reloads;
#      desktop + mobile screenshots
#   5. switch-pending catalog coalescing: the lane seeds a second native
#      session and holds the first switch_session frame OFF the wire >=12s
#      (crossing the 8s poll) -> ZERO session_list frames and zero
#      `session switch pending` text anywhere; the settled switch sends
#      exactly ONE deferred session_list and the target becomes active
#
# The playwright lane synchronizes through two marker files in the scenario
# work dir: readygate_spawn_test.mjs writes kill-server.marker; the lane
# kills + respawns rpi on the same port and writes server-up.marker.
#
# Browser driver: playwright via npm (ephemeral install) with a system
# Chrome/Chromium binary or playwright's bundled chromium. Missing node, a
# failed playwright install, or no usable Chromium FAILS the lane (exit 1,
# setup); assertion failures exit 2+.
#
# Usage: bash E2E.d/web/readygate_spawn.sh [run|list]
set -euo pipefail
SCRIPT_DIR="$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
E2E_DIR="$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd -P)"
# shellcheck source=../lib/common.sh
. "$E2E_DIR/lib/common.sh"
# shellcheck source=lib/fixture.sh
. "$SCRIPT_DIR/lib/fixture.sh"

SCENARIO="web-readygate-spawn"
TOKEN="web-readygate-spawn-e2e-token-$$-$(date +%s)"

# ---------------------------------------------------------------------------
main() {
    case "${1:-run}" in
        list|--list|--dry-run)
            printf '%s\n' \
                'web-readygate-spawn - WS delayed-open/reconnect sidebar+panel loads: boot primary row, real server kill -> reconnecting with ZERO .session-sidebar__error nodes and no sidebar error strings anywhere during the outage (crossing the 8s sidebar poll, rows retained), respawn on same port -> sidebar + session panel eventually load without user action, error surfaces absent post-reconnect; New Session footer entry opens the panel; held switch_session crossing the 8s poll sends ZERO session_list and renders zero "session switch pending", settled switch sends exactly one deferred session_list + desktop/mobile screenshots (PLAYWRIGHT-ONLY, hard-fail)'
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


    # Seed one synthetic native saved session (midnight timestamp: it always
    # sorts OLDER than the listener primary so `--continue` keeps resuming
    # the real primary) so the sidebar has a second row to switch to.
    python3 - "$root/home" "$root/workspace" <<'PY'
import datetime, json, pathlib, sys

home = pathlib.Path(sys.argv[1])
cwd = pathlib.Path(sys.argv[2])
sid = "switch-pending-seed"

encoded = "--" + str(cwd).lstrip("/").replace("/", "-").replace("\\", "-").replace(":", "-") + "--"
d = home / ".rpi" / "sessions" / encoded
d.mkdir(parents=True, exist_ok=True)

def line(record):
    return json.dumps(record, separators=(",", ":")) + "\n"

stamp = "2026-01-01T00:00:00Z"
records = [
    {"type": "session", "version": 3, "id": sid, "timestamp": stamp, "cwd": str(cwd)},
    {"type": "session_info", "id": "si-1", "parentId": None, "timestamp": stamp, "name": "Switch Pending Seed"},
    {"type": "message", "id": "m1", "parentId": None, "timestamp": stamp,
     "message": {"role": "user", "content": [{"type": "text", "text": "switch pending seed turn"}], "timestamp": 1}},
]
(d / "2026-01-01T000000-switch-pending-seed.jsonl").write_text("".join(line(r) for r in records), encoding="utf-8")
PY

    port="$(MOCK_SCENARIO=sessions web_start_mock "$root" "$evidence")"
    web_spawn_rpi "$root" "$evidence" "$port"
    url="$(web_wait_for_listener "$evidence")"
    log "$SCENARIO: listener at $url"

    local pw_status=0 pw_pid=0
    web_run_playwright "$url" "$evidence" "$root/playwright" "$SCRIPT_DIR/readygate_spawn_test.mjs" \
        RPI_WORK="$root/playwright" \
        >"$evidence/playwright.out" 2>&1 &
    pw_pid=$!

    # Wait for the test's kill request; respawn the listener on the SAME port.
    local deadline=$((SECONDS + 150))
    while [ ! -f "$root/playwright/kill-server.marker" ] && [ "$SECONDS" -lt "$deadline" ]; do
        if ! kill -0 "$pw_pid" 2>/dev/null; then break; fi
        sleep 0.2
    done
    if [ -f "$root/playwright/kill-server.marker" ]; then
        local listen_port url2
        listen_port="$(printf '%s' "$url" | sed -n 's#http://[^:]*:\([0-9]*\)/web#\1#p')"
        log "$SCENARIO: test requested a server kill — killing rpi (port $listen_port), respawning"
        web_kill_rpi
        sleep 1
        # --continue: resume the same session id so the reconnected sidebar
        # keeps the same identity (the gated poll must repopulate it).
        WEB_SPAWN_CONTINUE=1 web_spawn_rpi "$root" "$evidence" "$port" "$listen_port" "2"
        url2="$(web_wait_for_listener "$evidence" "2")"
        log "$SCENARIO: listener respawned at $url2"
        touch "$root/playwright/server-up.marker"
    fi

    wait "$pw_pid" || pw_status=$?
    if [ "$pw_status" -eq 0 ]; then
        printf 'playwright: delayed-open/reconnect sidebar+panel loads with zero sidebar error surfaces + desktop/mobile screenshots: PASSED\n' | tee "$evidence/playwright-summary.txt"
    elif [ "$pw_status" -eq 1 ]; then
        fail "$SCENARIO: playwright SETUP FAILED (node/chromium/npm unavailable)"
    else
        fail "$SCENARIO: playwright lane failed (exit $pw_status)"
    fi

    web_sanity_http "$url"
    web_finish_lane "$SCENARIO"
}

main "$@"
