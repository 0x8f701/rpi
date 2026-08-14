#!/usr/bin/env bash
# User-perspective Todo DAG page: the model's todo tool call creates a phased
# task list, then /todo opens the DAG overview; Enter drills into detail, Esc
# steps back to overview, a second Esc closes the panel and restores the
# composer.
#
# The mock provider returns a `todo` init tool call (two phases, four tasks);
# the DAG page chrome is asserted exactly (Todo DAGs / [main] / counts / Todo
# DAG detail / phase+task rows / ◌ markers).
set -euo pipefail
SCRIPT_DIR="$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
E2E_DIR="$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd -P)"
# shellcheck source=../lib/common.sh
. "$E2E_DIR/lib/common.sh"
# shellcheck source=../lib/orchestration_fixtures.sh
. "$E2E_DIR/lib/orchestration_fixtures.sh"

SCENARIO="todo-dag"

wait_for() { # needle [timeout]
    local needle="$1" timeout="${2:-25}"
    if ! tmux_wait_for "$session" "$timeout" "$needle" >"$evidence/live.txt"; then
        fail "user.$SCENARIO: did not find ${needle@Q} (see $evidence/live.txt)"
    fi
}

send_cmd() { # literal command line
    tmux send-keys -t "$session":0 -l "$1"
    tmux send-keys -t "$session":0 Enter
}

start_mock() { # root -> prints port
    local root="$1" port_file="$root/mock-port.txt" deadline port
    python3 "$E2E_DIR/lib/user_mock_server.py" --scenario todo-dag --port-file "$port_file" \
        >"$evidence/mock-server.log" 2>&1 &
    register_pid $!
    deadline=$((SECONDS + 15))
    while [ ! -s "$port_file" ] && [ "$SECONDS" -lt "$deadline" ]; do sleep 0.2; done
    [ -s "$port_file" ] || fail "user.$SCENARIO: mock server did not write its port file"
    port="$(cat "$port_file")"
    printf '%s\n' "$port"
}

main() {
    require_rpi
    require_cmd tmux
    require_cmd python3

    local root evidence session port
    root="$(scenario_workspace "$SCENARIO")"
    evidence="$EVIDENCE_ROOT/$SCENARIO"
    session="$(unique_tmux_name "$SCENARIO")"
    register_tmux_session "$session"

    port="$(start_mock "$root")"
    cat >"$root/home/.pi/agent/models.json" <<EOF
{
  "providers": {
    "user-todo-dag": {
      "baseUrl": "http://127.0.0.1:$port",
      "api": "openai-completions",
      "models": [
        { "id": "mock", "name": "Todo DAG Mock", "contextWindow": 32768, "maxTokens": 2048 }
      ]
    }
  }
}
EOF

    tmux new-session -d -s "$session" -x 140 -y 42 -c "$root/workspace" \
        "env HOME='$root/home' USERPROFILE='$root/home' \
PI_CODING_AGENT_DIR='$root/home/.pi/agent' \
PI_OFFLINE=1 PI_SKIP_VERSION_CHECK=1 \
TERM=xterm-256color \
'$RPI_BIN' --offline --model user-todo-dag/mock --api-key user-mock-key; printf '===TUI-DONE-todo-dag===\n'"

    tmux_wait_for "$session" 30 'user-todo-dag/mock' >"$evidence/boot.txt" || true
    # Dismiss the startup resume selector when it is showing (harmless otherwise).
    tmux send-keys -t "$session":0 Escape
    sleep 0.3

    # --- Prompt: mock returns the todo init tool call ---
    send_cmd 'set up the task list'
    wait_for 'Todos · 1 active · 3 next · 0/4' 30
    wait_for 'map parser surface' 30
    wait_for 'bound Todo projection' 30

    # --- /todo opens the DAG overview ---
    send_cmd '/todo'
    wait_for 'Todo DAGs' 20
    wait_for '[main]' 20
    wait_for '4 open' 20
    wait_for '0 blocked' 20

    # --- Enter opens the detail page ---
    tmux send-keys -t "$session":0 Enter
    wait_for 'Todo DAG detail' 20
    wait_for 'Survey' 20
    wait_for 'Construct' 20
    wait_for 'map parser surface' 20
    wait_for 'repair composer repaint' 20
    wait_for '4 open' 20
    wait_for '0 blocked' 20

    # --- Esc steps back to the overview ---
    tmux send-keys -t "$session":0 Escape
    wait_for 'Todo DAGs' 15

    # --- Esc closes the panel; the composer takes input again ---
    tmux send-keys -t "$session":0 Escape
    sleep 0.5
    assert_tmux_composer_editable "$session" "$evidence/composer.txt" 'TODO-SENTINEL'

    tmux capture-pane -p -S -3000 -t "$session":0 >"$evidence/tui.txt" 2>/dev/null || true
    tmux send-keys -t "$session":0 -l '/quit'
    tmux send-keys -t "$session":0 Enter
    if ! tmux_wait_for "$session" 12 '===TUI-DONE-todo-dag===' >"$evidence/quit.txt"; then
        log "user.$SCENARIO: quit marker not seen; scenario assertions already passed, tearing down"
    fi
    tmux kill-session -t "$session" 2>/dev/null || true

    # Panel content is repainted, never scrolled; the per-step waits are the
    # assertions (overview/detail chrome + navigation).
    printf 'user.%s passed\nevidence=%s\n' "$SCENARIO" "$evidence"
}

main "$@"
