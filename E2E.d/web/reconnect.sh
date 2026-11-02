#!/usr/bin/env bash
# Web auto-reconnect E2E lane (playwright-only hard gate).
#
# Spawns the real `rpi --listen` binary with a token file and the loopback
# mock provider (--scenario steering; request 1 streams slowly, request 2 is
# instant), opens `/web` in a real browser, and asserts the reconnect
# contract against a REAL server crash:
#   1. the WS connects and a first prompt round-trips (slow reply)
#   2. the lane KILLS the rpi listener mid-session -> the transport outage
#      toast appears on its own (App.tsx scheduleReconnect; the #conn-state
#      pill is removed — the transport toast is the outage authority)
#   3. the lane respawns the listener on the SAME port -> the client
#      auto-reconnects (toast clears) without any user action
#   4. the pre-crash transcript survives the reconnect
#   5. a prompt after the reconnect round-trips (request 2, instant reply)
#
# The playwright lane synchronizes with the lane script through three marker
# files in the scenario work dir: reconnect_test.mjs writes
# kill-server.marker; after observing the real 1006 it writes
# outage-observed.marker; the lane respawns rpi and writes server-up.marker.
#
# Browser driver: playwright via npm (ephemeral install in the scenario work
# dir) with a system Chrome/Chromium binary or playwright's bundled chromium.
# Missing node, a failed playwright install, or no usable Chromium FAILS the
# lane (exit 1, setup); assertion failures exit 2+.
#
# Usage: bash E2E.d/web/reconnect.sh [run|list]
set -euo pipefail
SCRIPT_DIR="$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
E2E_DIR="$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd -P)"
# shellcheck source=../lib/common.sh
. "$E2E_DIR/lib/common.sh"
# shellcheck source=lib/fixture.sh
. "$SCRIPT_DIR/lib/fixture.sh"

SCENARIO="web-reconnect"
TOKEN="web-reconnect-e2e-token-$$-$(date +%s)"
FIRST_TAIL="chunk-four-done"     # tail of the FIRST (slow) mock stream
FAST_REPLY="steering-followup-reply"

# ---------------------------------------------------------------------------
main() {
    case "${1:-run}" in
        list|--list|--dry-run)
            printf '%s\n' \
                'web-reconnect - real listener kill -> observed 1006 + repeated failed reconnects -> same-port recovery with one outage toast, zero sidebar/panel transport errors, retained session/panel/transcript, post-recovery prompt; injected non-transport session_list RPC-limit failure retains the catalog with zero .session-sidebar__error and no sidebar error strings + desktop/mobile screenshots (playwright, hard gate)'
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
    cat >"$root/home/.rpi/settings.json" <<'EOF'
{
  "orchestration": {
    "tasks": true,
    "maxConcurrency": 2,
    "maxRecursionDepth": 2
  }
}
EOF
    mkdir -p "$root/home/.rpi/agents"
    cat >"$root/home/.rpi/agents/writer.md" <<'EOF'
---
name: writer
description: Deterministic restart worker
tools:
  - read
---
You are the writer agent for durable reconnect lifecycle verification.
EOF

    # Keep the fixture under the 256 KiB preview cap so the tree row stays
    # clickable; the first paged read still races the real listener kill.
    python3 - "$root/workspace/000-reconnect-large.txt" <<'PY'
import sys

path = sys.argv[1]
line = "reconnect-file-line\n"
target = 200 * 1024
with open(path, "w", encoding="utf-8") as output:
    written = 0
    while written < target:
        output.write(line)
        written += len(line)
PY

    port="$(web_start_mock "$root" "$evidence")"
    web_spawn_rpi "$root" "$evidence" "$port"
    url="$(web_wait_for_listener "$evidence")"
    log "$SCENARIO: listener at $url"

    local pw_status=0 pw_pid=0
    web_run_playwright "$url" "$evidence" "$root/playwright" "$SCRIPT_DIR/reconnect_test.mjs" \
        RPI_FIRST_TAIL="$FIRST_TAIL" RPI_FAST_REPLY="$FAST_REPLY" RPI_POST_RESTART_REPLY="reconnect-post-restart-reply" RPI_WORK="$root/playwright" RPI_MOCK_PORT="$port" \
        >"$evidence/playwright.out" 2>&1 &
    pw_pid=$!

    # Wait for the real listener kill to produce close code 1006. The browser
    # then drives three event-observed constructor failures through the one
    # product reconnect/backoff state machine while this shell restores the
    # same port; those synthetic pre-TCP failures keep Chromium's console
    # clean without weakening the repeated-attempt contract.
    local deadline=$((SECONDS + 120))
    while [ ! -f "$root/playwright/kill-server.marker" ] && [ "$SECONDS" -lt "$deadline" ]; do
        if ! kill -0 "$pw_pid" 2>/dev/null; then break; fi
        sleep 0.01
    done
    if [ -f "$root/playwright/kill-server.marker" ]; then
        local listen_port url2 outage_deadline
        listen_port="$(printf '%s' "$url" | sed -n 's#http://[^:]*:\([0-9]*\)/web#\1#p')"
        log "$SCENARIO: test requested a real server kill on port $listen_port"
        web_kill_rpi
        curl -s -X POST "http://127.0.0.1:$port/__release-steering" -d '{}' >/dev/null 2>&1 || true
        outage_deadline=$((SECONDS + 60))
        while [ ! -f "$root/playwright/outage-observed.marker" ] && [ "$SECONDS" -lt "$outage_deadline" ]; do
            kill -0 "$pw_pid" 2>/dev/null || break
            sleep 0.1
        done
        [ -f "$root/playwright/outage-observed.marker" ] \
            || fail "$SCENARIO: browser did not prove the real listener close code 1006"
        WEB_SPAWN_RESUME="$(cat "$root/playwright/listener-primary-session-id")" web_spawn_rpi "$root" "$evidence" "$port" "$listen_port" "2"
        url2="$(web_wait_for_listener "$evidence" "2")"
        log "$SCENARIO: listener recovered at $url2"
        touch "$root/playwright/server-up.marker"
    fi

    wait "$pw_pid" || pw_status=$?
    if [ "$pw_status" -eq 0 ]; then
        printf 'playwright: real 1006 outage with in-flight File read recovery, three failed product reconnect generations, single transport toast, retained panel/session/transcript, same-port recovery, post-recovery A→B→A + prompt, zero browser errors: PASSED\n' | tee "$evidence/playwright-summary.txt"
    elif [ "$pw_status" -eq 1 ]; then
        fail "$SCENARIO: playwright SETUP FAILED (node/chromium/npm unavailable)"
    else
        fail "$SCENARIO: playwright lane failed (exit $pw_status)"
    fi

    web_sanity_http "$url"
    web_finish_lane "$SCENARIO"
}

main "$@"
