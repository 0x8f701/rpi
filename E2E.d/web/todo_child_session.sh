#!/usr/bin/env bash
# Web Todo-child session E2E lane (playwright-only hard gate).
#
# Spawns the real `rpi --listen` binary with a token file and the loopback
# mock provider (--scenario sessions; content-routed replies so parent and
# child prompts stay deterministic), opens `/web` in a real Playwright
# browser, and drives the REAL Todo children panel (see
# todo_child_session_test.mjs). No fake backend state:
#   1. typed RPCs stay distinct: todo_child_create / list / select / send /
#      add_task / hub_send / pause / resume / stop
#   2. parent cleanup and a browser WebSocket drop do not stop the child
#   3. a real-listener request with a wrong parentSessionId+childSessionId
#      pairing fails closed; the selected child projection stays unchanged
#   4. ordinary Todo / Activity / Workflow surfaces stay distinct
#   5. a REAL listener kill -> transport outage toast -> same-port respawn ->
#      durable child A recovers after the client auto-reconnects
#
# The playwright half synchronizes through two marker files in the scenario
# work dir: todo_child_session_test.mjs writes kill-server.marker; this lane
# stops ONLY the listener host, respawns rpi on the SAME port, waits for the
# control-plane banner, and writes server-up.marker only after that readiness
# check. The marker is never written on a missed kill request, a dead
# playwright child, a missing listen port, or a listener that failed to bind.
#
# Browser driver: playwright via npm (ephemeral install in the scenario work
# dir) with a system Chrome/Chromium binary or playwright's bundled chromium.
# Missing node, a failed playwright install, or no usable Chromium FAILS the
# lane (exit 1, setup); assertion failures exit 2+.
#
# Usage: bash E2E.d/web/todo_child_session.sh [run|list]
set -euo pipefail
SCRIPT_DIR="$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
E2E_DIR="$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd -P)"
# shellcheck source=../lib/common.sh
. "$E2E_DIR/lib/common.sh"
# shellcheck source=lib/fixture.sh
. "$SCRIPT_DIR/lib/fixture.sh"

SCENARIO="web-todo_child_session"
TOKEN="web-todo-child-e2e-token-$$-$(date +%s)"

live_todo_child_routing() {
    SCENARIO="todo-child-routing-web"
    require_rpi
    web_require_browser
    require_cmd python3

    local root evidence url pw_status=0
    prepare_roots
    root="$WORK_ROOT/$SCENARIO"
    evidence="$EVIDENCE_ROOT/$SCENARIO"
    rm -rf "$root"
    mkdir -p "$root/workspace/.rpi" "$root/sessions" "$root/playwright" "$evidence"
    cat >"$root/workspace/.rpi/settings.json" <<'EOF'
{"defaultProvider":"cliproxy","defaultModel":"gpt-5.6-sol","defaultProjectTrust":"always","selector":{"autoMode":"auto"},"orchestration":{"tasks":true,"todo":true,"maxConcurrency":4,"maxRecursionDepth":2}}
EOF
    register_cleanup_path "$root"
    web_prepare_dist "$WORK_ROOT/web-dist"

    (
        cd "$root/workspace"
        exec "$RPI_BIN" -C "$root/workspace" --approve --session-dir "$root/sessions" --session-id todo-web-parent \
            --listen 127.0.0.1:0 --listen-plaintext \
            >"$evidence/rpi.stdout" 2>"$evidence/rpi.stderr"
    ) &
    RPI_PID=$!
    register_pid "$RPI_PID"
    url="$(web_wait_for_listener "$evidence")"

    TOKEN="" web_run_playwright "$url" "$evidence" "$root/playwright" "$SCRIPT_DIR/todo_child_session_test.mjs" \
        RPI_TODO_CHILD_LIVE=1 RPI_WORK="$root" \
        >"$evidence/playwright.out" 2>&1 || pw_status=$?
    [ "$pw_status" -eq 0 ] || fail "$SCENARIO: Playwright live lane failed (exit $pw_status; see $evidence/playwright.out)"
    local parent_id child_id child_file
    parent_id="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["parentSessionId"])' "$evidence/web-evidence.json")"
    child_id="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["childSessionId"])' "$evidence/web-evidence.json")"
    child_file="$root/sessions/children/$parent_id/$child_id.jsonl"
    [ -f "$child_file" ] || fail "$SCENARIO: live child journal missing before shutdown"
    local deadline
    deadline=$((SECONDS + 60))
    while ((SECONDS < deadline)); do
        if python3 "$E2E_DIR/live/verify.py" mutate-running-receipt "$child_file" >"$evidence/shutdown-receipt-id.txt" 2>"$evidence/shutdown-receipt-wait.txt"; then
            break
        fi
        sleep 0.2
    done
    [ -s "$evidence/shutdown-receipt-id.txt" ] \
        || fail "$SCENARIO: shutdown receipt did not remain running long enough for the immutable-record gate"
    kill -TERM "$RPI_PID" 2>/dev/null || true
    wait "$RPI_PID" 2>/dev/null || true
    RPI_PID=""
    python3 - "$evidence/web-evidence.json" "$evidence/rpi.stderr" <<'PY'
from pathlib import Path
import json, sys
path = Path(sys.argv[1]); value = json.loads(path.read_text(encoding="utf-8"))
stderr = Path(sys.argv[2]).read_text(encoding="utf-8", errors="replace")
if "Todo child shutdown failed" not in stderr or "Todo child receipt immutable fields changed" not in stderr:
    raise SystemExit("Web running-receipt CAS conflict did not propagate through listener shutdown")
value["shutdownFailurePropagated"] = True
value["shutdownFailureContext"] = "Todo child receipt immutable fields changed"
path.write_text(json.dumps(value, indent=2) + "\n", encoding="utf-8")
PY
    printf '%s passed\nevidence=%s\n' "$SCENARIO" "$evidence"
}

# ---------------------------------------------------------------------------
main() {
    case "${1:-run}" in
        --live|live) live_todo_child_routing; return 0 ;;
        list|--list|--dry-run)
            printf '%s\n' \
                'web-todo_child_session - deterministic mock Todo-child browser gate' \
                'todo-child-routing-web - application-loaded public-model Todo routing browser gate'
            return 0
            ;;
        run|all) ;;
        *) fail "usage: $0 [run|--live|list]" ;;
    esac

    require_rpi
    web_require_browser

    local root evidence port url
    prepare_roots
    scenario_workspace "$SCENARIO" >/dev/null
    root="$WORK_ROOT/$SCENARIO"
    register_cleanup_path "$root"
    evidence="$EVIDENCE_ROOT/$SCENARIO"
    printf '%s\n' "$TOKEN" >"$root/token"

    require_cmd git
    git -C "$root/workspace" init -q
    git -C "$root/workspace" config user.email "web-todo-child@example.com"
    git -C "$root/workspace" config user.name "Web Todo Child"
    git -C "$root/workspace" config commit.gpgsign false
    printf 'web todo-child seed\n' >"$root/workspace/seed.txt"
    git -C "$root/workspace" add -- seed.txt
    git -C "$root/workspace" commit -q -m "web todo-child seed"

    cat >"$root/home/.rpi/settings.json" <<'EOF'
{
  "orchestration": {
    "tasks": true,
    "todo": true,
    "maxConcurrency": 2,
    "maxRecursionDepth": 2,
    "maxToolsPerAgent": 32,
    "isolation": "none"
  }
}
EOF

    port="$(MOCK_SCENARIO=sessions web_start_mock "$root" "$evidence")"
    local mock_pid
    read -r mock_pid _ <"$root/mock.pid" 2>/dev/null || true
    case "$mock_pid" in
        ''|*[!0-9]*) fail "$SCENARIO: mock.pid missing or not a pid after web_start_mock" ;;
    esac
    register_pid "$mock_pid"
    web_spawn_rpi "$root" "$evidence" "$port"
    url="$(web_wait_for_listener "$evidence")"
    log "$SCENARIO: listener at $url"

    mkdir -p "$root/playwright"
    rm -f "$root/playwright/kill-server.marker" "$root/playwright/server-up.marker"

    local pw_status=0 pw_pid=0
    web_run_playwright "$url" "$evidence" "$root/playwright" "$SCRIPT_DIR/todo_child_session_test.mjs" \
        RPI_WORK="$root/playwright" \
        >"$evidence/playwright.out" 2>&1 &
    pw_pid=$!
    register_pid "$pw_pid"

    # Wait for the test's kill request; respawn the listener on the SAME port.
    # Fail closed: never write server-up.marker unless the new listener's
    # control-plane banner is observed on that port.
    local deadline=$((SECONDS + 300))
    while [ ! -f "$root/playwright/kill-server.marker" ] && [ "$SECONDS" -lt "$deadline" ]; do
        if ! kill -0 "$pw_pid" 2>/dev/null; then
            fail "$SCENARIO: playwright exited before requesting the restart (see $evidence/playwright.out)"
        fi
        sleep 0.2
    done
    if [ ! -f "$root/playwright/kill-server.marker" ]; then
        fail "$SCENARIO: test never wrote the kill-server marker"
    fi

    local listen_port url2
    listen_port="$(printf '%s' "$url" | sed -n 's#http://[^:]*:\([0-9]*\)/web#\1#p')"
    [ -n "$listen_port" ] || fail "$SCENARIO: could not parse listen port from $url"
    log "$SCENARIO: test requested a server kill — killing rpi (port $listen_port), respawning"
    web_kill_rpi
    sleep 1
    # --continue: resume the same parent session id so durable Todo children
    # stay bound to the auto-reconnected Web client.
    WEB_SPAWN_CONTINUE=1 web_spawn_rpi "$root" "$evidence" "$port" "$listen_port" "2"
    url2="$(web_wait_for_listener "$evidence" "2")"
    [ "$url2" = "$url" ] || fail "$SCENARIO: respawned listener bound a different URL ($url2 vs $url)"
    log "$SCENARIO: listener respawned at $url2"
    printf 'up\n' >"$root/playwright/server-up.marker"

    wait "$pw_pid" || pw_status=$?
    if [ "$pw_status" -eq 0 ]; then
        printf 'playwright: Todo-child typed RPCs, isolation, WS drop, durable restart: PASSED\n' | tee "$evidence/playwright-summary.txt"
    elif [ "$pw_status" -eq 1 ]; then
        fail "$SCENARIO: playwright SETUP FAILED (node/chromium/npm unavailable)"
    else
        fail "$SCENARIO: playwright lane failed (exit $pw_status) — see $evidence/playwright.out"
    fi

    web_sanity_http "$url"
    web_finish_lane "$SCENARIO"
    if [ -e "$root/stdin" ] || [ -e "$root/stdin2" ]; then
        fail "$SCENARIO: stdin or stdin2 FIFO remained after lane exit"
    fi
}

main "$@"
