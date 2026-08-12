#!/usr/bin/env bash
# Web session restore + persistence E2E lane (playwright-only, hard-fail).
#
# Drives the real `rpi --listen` Web-only binary + loopback mock (scenario
# `sessions`, instant content-routed replies) through a real Playwright
# browser and asserts (see session_restore_test.mjs):
#   R0  a persisted fixture restores legitimate assistant/IRC content, hides
#       display:false internal customs, and bounds bash/tool output
#   R1  a Web prompt round-trips and the new session's row appears
#   R2  switching away/back to a LOADED session restores its transcript
#       from the authoritative backend snapshot
#   R4  after SIGTERM-restarting the listener, reopening the Web and
#       switching to the recorded session restores its history from disk
#   R5  a page reload restores the last-activated session from the selected
#       listener's preference, including a page/listener authority mismatch
#   R6  a saved listener preference naming a nonexistent session falls back
#       to the first catalog row, which becomes active with its transcript
#
# The lane FAILS (non-zero) when playwright/chromium cannot be used; there
# is no agent-browser fallback and no skip.
#
# Usage: bash E2E.d/web/session_restore.sh [run|list]
set -euo pipefail
SCRIPT_DIR="$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
E2E_DIR="$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd -P)"
# shellcheck source=../lib/common.sh
. "$E2E_DIR/lib/common.sh"
# shellcheck source=lib/fixture.sh
. "$SCRIPT_DIR/lib/fixture.sh"

SCENARIO="web-session-restore"
TOKEN="web-session-restore-e2e-token-$$-$(date +%s)"
MOCK_SCENARIO="sessions"

# ---------------------------------------------------------------------------
main() {
    case "${1:-run}" in
        list|--list|--dry-run)
            printf '%s\n' \
                'web-session-restore - session parity: prompt recorded, loaded switch restore, restart restore, selected-listener preference reload restore, missing-preference fallback (PLAYWRIGHT-ONLY, hard-fail)'
            return 0
            ;;
        run|all) ;;
        *) fail "usage: $0 [run|list]" ;;
    esac

    require_rpi
    web_require_browser

    local root evidence port url listen_port pw_status=0 pw_pid=0
    root="$(scenario_workspace "$SCENARIO")"
    evidence="$EVIDENCE_ROOT/$SCENARIO"
    session_dir="$root/sessions"
    python3 - "$root/home/.pi/agent/settings.json" "$session_dir" <<'PY'
import json, sys
open(sys.argv[1], 'w', encoding='utf-8').write(json.dumps({'sessionDir': sys.argv[2]}) + '\n')
PY
    printf '%s\n' "$TOKEN" >"$root/token"

    # Persist a native session before listener startup so the browser exercises
    # the restored transcript path against real recorder wire shapes. The
    # fixture includes legitimate assistant/IRC content, a hidden internal
    # custom, long legacy output, and durable bash/generic toolCall/result
    # pairs whose metadata, terminal state, and bounded tails must restore.
    python3 - "$session_dir" "$root/workspace" <<'PY'
import datetime, json, pathlib, sys
session_dir = pathlib.Path(sys.argv[1])
cwd = sys.argv[2]
session_dir.mkdir(parents=True, exist_ok=True)
stamp = datetime.datetime.now(datetime.timezone.utc).isoformat().replace('+00:00', 'Z')
sid = 'web-transcript-parity'
records = [
    {'type': 'session', 'version': 3, 'id': sid, 'timestamp': stamp, 'cwd': cwd},
    # Explicit session_info name: the Web sidebar `temporary` marker must
    # never apply to this legal restored session (native + unnamed + <10KiB +
    # cwd under the OS temp root would otherwise hide it by default).
    {'type': 'session_info', 'id': 'si-1', 'parentId': None, 'timestamp': stamp,
     'name': 'web-transcript-parity'},
    {'type': 'message', 'id': 'u1', 'parentId': None, 'timestamp': stamp,
     'message': {'role': 'user', 'content': [{'type': 'text', 'text': 'transcript parity seed'}], 'timestamp': 1}},
    {'type': 'custom_message', 'id': 'hidden', 'parentId': 'u1', 'timestamp': stamp,
     'customType': 'pi.goal.active', 'content': '<system-reminder>internal parity secret</system-reminder>',
     'display': False, 'details': {}},
    {'type': 'custom_message', 'id': 'irc', 'parentId': 'hidden', 'timestamp': stamp,
     'customType': 'orchestration_message',
     'content': '<orchestration-message id="m1" from="Main">\nclean IRC parity body\nReplying to message: parent-9\n</orchestration-message>',
     'display': True, 'details': {'id': 'm1', 'from': 'Main', 'to': 'Child', 'body': 'clean IRC parity body', 'replyTo': 'parent-9'}},
    {'type': 'custom_message', 'id': 'irc-in', 'parentId': 'irc', 'timestamp': stamp,
     'customType': 'orchestration_message',
     'content': '<orchestration-message id="m2" from="Child">\n' + '\n'.join(f'irc-incoming-line-{n}' for n in range(1, 11)) + '\n</orchestration-message>',
     'display': True, 'details': {'id': 'm2', 'from': 'Child', 'to': 'Main', 'body': '\n'.join(f'irc-incoming-line-{n}' for n in range(1, 11))}},
    {'type': 'message', 'id': 'bash', 'parentId': 'irc-in', 'timestamp': stamp,
     'message': {'role': 'bashExecution', 'command': 'seq 1 30',
                 'output': '\n'.join(f'bash-parity-{n}' for n in range(1, 31)),
                 'cancelled': False, 'truncated': False, 'timestamp': 2}},
    {'type': 'message', 'id': 'bash-call', 'parentId': 'bash', 'timestamp': stamp,
     'message': {'role': 'assistant',
                 'content': [{'type': 'toolCall', 'id': 'parity-bash-card', 'name': 'bash',
                              'arguments': {'command': 'seq 1 15'}}],
                 'api': 'faux', 'provider': 'faux', 'model': 'faux-1', 'stopReason': 'toolUse', 'timestamp': 3}},
    {'type': 'message', 'id': 'bash-result', 'parentId': 'bash-call', 'timestamp': stamp,
     'message': {'role': 'toolResult', 'toolCallId': 'parity-bash-card', 'toolName': 'bash',
                 'content': [{'type': 'text', 'text': '\n'.join(f'bash-card-{n}' for n in range(1, 16))}],
                 'isError': False, 'timestamp': 4}},
    {'type': 'message', 'id': 'tool-call', 'parentId': 'bash-result', 'timestamp': stamp,
     'message': {'role': 'assistant',
                 'content': [{'type': 'toolCall', 'id': 'parity-tool', 'name': 'read',
                              'arguments': {'path': 'parity.txt'}}],
                 'api': 'faux', 'provider': 'faux', 'model': 'faux-1', 'stopReason': 'toolUse', 'timestamp': 5}},
    {'type': 'message', 'id': 'tool', 'parentId': 'tool-call', 'timestamp': stamp,
     'message': {'role': 'toolResult', 'toolCallId': 'parity-tool', 'toolName': 'read',
                 'content': [{'type': 'text', 'text': '\n'.join(f'tool-parity-{n}' for n in range(1, 13))}],
                 'isError': True, 'timestamp': 6}},
    {'type': 'message', 'id': 'orphan-tool', 'parentId': 'tool', 'timestamp': stamp,
     'message': {'role': 'toolResult', 'toolCallId': 'orphan-parity-tool', 'toolName': 'grep',
                 'content': [{'type': 'text', 'text': '\n'.join(f'orphan-parity-{n}' for n in range(1, 9))}],
                 'isError': False, 'timestamp': 7}},
    {'type': 'message', 'id': 'a1', 'parentId': 'orphan-tool', 'timestamp': stamp,
     'message': {'role': 'assistant', 'content': [{'type': 'text', 'text': 'system-reminder wording is legitimate assistant text'}],
                 'api': 'faux', 'provider': 'faux', 'model': 'faux-1', 'stopReason': 'stop', 'timestamp': 8}},
]
(session_dir / f'{stamp[:10]}T000000-{sid}.jsonl').write_text(
    ''.join(json.dumps(record, separators=(',', ':')) + '\n' for record in records),
    encoding='utf-8',
)
PY

    # A FIXED listen port so the respawned listener reuses it (the browser
    # reconnects to the same URL without a page reload).
    listen_port="$({ python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1]); s.close()' 2>/dev/null; } | tr -d '[:space:]')"
    [ -n "$listen_port" ] || fail "could not allocate a fixed listen port"

    port="$(MOCK_SCENARIO="$MOCK_SCENARIO" web_start_mock "$root" "$evidence")"
    web_spawn_rpi "$root" "$evidence" "$port" "$listen_port"
    url="$(web_wait_for_listener "$evidence")"
    log "$SCENARIO: listener at $url"

    rm -f "$root/playwright/kill-server.marker" "$root/playwright/server-up.marker"
    mkdir -p "$root/playwright"

    web_run_playwright "$url" "$evidence" "$root/playwright" "$SCRIPT_DIR/session_restore_test.mjs" \
        "RPI_REPLY=sessions-reply:" \
        "RPI_PARITY_SESSION=web-transcript-parity" \
        >"$evidence/playwright.out" 2>&1 &
    pw_pid=$!
    # Register the playwright child so the EXIT/HUP/INT/TERM trap reaps it
    # if the lane is killed externally (e.g. by a wrapping timeout). pw_pid
    # is a function-local otherwise unreaped by cleanup_e2e; a stuck child
    # (here: the test's setInterval kept Node alive) would leak on interrupt.
    register_pid "$pw_pid"

    # Wait for the test's restart request; respawn the listener on the SAME port.
    local deadline=$((SECONDS + 180))
    while [ ! -f "$root/playwright/kill-server.marker" ] && [ "$SECONDS" -lt "$deadline" ]; do
        if ! kill -0 "$pw_pid" 2>/dev/null; then
            fail "$SCENARIO: playwright exited before requesting the restart (see $evidence/playwright.out)"
        fi
        sleep 0.3
    done
    if [ ! -f "$root/playwright/kill-server.marker" ]; then
        fail "$SCENARIO: test never wrote the kill-server marker"
    fi

    log "$SCENARIO: restarting the listener (SIGTERM) on port $listen_port"
    web_kill_rpi
    sleep 1
    # Respawn WITHOUT --continue: a Web-only restart starts a fresh primary;
    # the recorded session is reopened by switching to it from the sidebar.
    web_spawn_rpi "$root" "$evidence" "$port" "$listen_port" "2"
    if ! web_wait_for_listener "$evidence" "2" >/dev/null 2>&1; then
        fail "$SCENARIO: respawned listener never came back (see $evidence/rpi2.stderr)"
    fi
    printf 'up\n' >"$root/playwright/server-up.marker"

    wait "$pw_pid" || pw_status=$?
    if [ "$pw_status" -ne 0 ]; then
        fail "$SCENARIO: playwright lane failed (exit $pw_status) — see $evidence/playwright.out"
    fi

    web_sanity_http "$url"
    web_finish_lane "$SCENARIO"
}

main "$@"