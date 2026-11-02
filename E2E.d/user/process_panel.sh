#!/usr/bin/env bash
# User-perspective Processes overlay: start a supervised command, open /ps,
# assert the list shows the running process, then quit.
#
# Runs the real rpi TUI in an isolated tmux session with the faux provider.
set -euo pipefail
SCRIPT_DIR="$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
E2E_DIR="$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd -P)"
# shellcheck source=../lib/common.sh
. "$E2E_DIR/lib/common.sh"
# shellcheck source=../lib/orchestration_fixtures.sh
. "$E2E_DIR/lib/orchestration_fixtures.sh"

SCENARIO="process-panel"

wait_for() {
    local needle="$1" timeout="${2:-25}"
    if ! tmux_wait_for "$session" "$timeout" "$needle" >"$evidence/live.txt"; then
        fail "user.$SCENARIO: did not find ${needle@Q} (see $evidence/live.txt)"
    fi
}

send_cmd() {
    tmux send-keys -t "$session":0 -l "$1"
    tmux send-keys -t "$session":0 Enter
}

main() {
    require_rpi
    require_cmd tmux

    local root evidence session
    root="$(scenario_workspace "$SCENARIO")"
    evidence="$EVIDENCE_ROOT/$SCENARIO"
    session="$(unique_tmux_name "$SCENARIO")"
    register_tmux_session "$session"

    tmux new-session -d -s "$session" -x 140 -y 42 -c "$root/workspace" \
        "env HOME='$root/home' USERPROFILE='$root/home' \
PI_CODING_AGENT_DIR='$root/home/.pi/agent' \
RPI_OFFLINE=1 RPI_SKIP_VERSION_CHECK=1 \
PI_FAUX_RESPONSE='deterministic-process-reply' \
TERM=xterm-256color \
'$RPI_BIN' --offline --model faux/faux-1; printf '===TUI-DONE-process-panel===\n'"

    tmux_wait_for "$session" 25 'faux/faux-1' >"$evidence/boot.txt" || true
    tmux send-keys -t "$session":0 Escape
    sleep 0.3

    send_cmd "/process start sh -c 'printf processes-e2e-ready; sleep 90'"
    wait_for 'Running' 20
    tmux capture-pane -p -S -200 -t "$session":0 >"$evidence/start.txt"

    send_cmd '/ps'
    wait_for 'Processes' 15
    tmux capture-pane -p -S -800 -t "$session":0 >"$evidence/ps-panel.txt"
    if ! grep -E -e 'Processes' "$evidence/ps-panel.txt" >/dev/null; then
        fail "user.$SCENARIO: /ps overlay title missing"
    fi
    if ! grep -E -ie 'running' "$evidence/ps-panel.txt" >/dev/null; then
        fail "user.$SCENARIO: /ps did not show a running process (see $evidence/ps-panel.txt)"
    fi

    tmux send-keys -t "$session":0 Escape
    sleep 0.3
    tmux capture-pane -p -S -3000 -t "$session":0 >"$evidence/tui.txt" 2>/dev/null || true
    tmux send-keys -t "$session":0 -l '/quit'
    tmux send-keys -t "$session":0 Enter
    if ! tmux_wait_for "$session" 12 '===TUI-DONE-process-panel===' >"$evidence/quit.txt"; then
        log "user.$SCENARIO: quit marker not seen; scenario assertions already passed, tearing down"
    fi
    tmux kill-session -t "$session" 2>/dev/null || true
    printf 'user.%s passed\nevidence=%s\n' "$SCENARIO" "$evidence"
}

main "$@"
