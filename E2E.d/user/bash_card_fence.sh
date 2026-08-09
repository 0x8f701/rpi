#!/usr/bin/env bash
# User-perspective TUI chrome: a bash tool card rendered from a multi-line
# command with leading comment lines (OMP-style comment frame), and a code
# fence that never closes (unclosed-fence marker on the frame bottom).
#
# The mock provider returns a real `bash` tool call first (the command runs in
# the workspace) and then assistant text whose ```rust fence stays open.
set -euo pipefail
SCRIPT_DIR="$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
E2E_DIR="$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd -P)"
# shellcheck source=../lib/common.sh
. "$E2E_DIR/lib/common.sh"
# shellcheck source=../lib/orchestration_fixtures.sh
. "$E2E_DIR/lib/orchestration_fixtures.sh"

SCENARIO="bash-card-fence"

wait_for() { # needle [timeout]
    local needle="$1" timeout="${2:-30}"
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
    python3 "$E2E_DIR/lib/user_mock_server.py" --scenario bash-card --port-file "$port_file" \
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
    "user-bash-card": {
      "baseUrl": "http://127.0.0.1:$port",
      "api": "openai-completions",
      "models": [
        { "id": "mock", "name": "Bash Card Mock", "contextWindow": 32768, "maxTokens": 2048 }
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
'$RPI_BIN' --offline --model user-bash-card/mock --api-key user-mock-key; printf '===TUI-DONE-bash-card-fence===\n'"

    tmux_wait_for "$session" 30 'user-bash-card/mock' >"$evidence/boot.txt" || true
    # Dismiss the startup resume selector when it is showing (harmless otherwise).
    tmux send-keys -t "$session":0 Escape
    sleep 0.3

    # --- Prompt 1: mock returns a bash tool call (multi-line + comments) ---
    send_cmd 'run the shell probe'
    # The bash tool executes for real: its output lands in the card.
    wait_for 'card-one' 40
    wait_for 'card-two' 40
    # Comment frame: first comment is the top border title, the second is a
    # frame row above the command.
    wait_for '# QA shell probe' 15
    wait_for '# second probe line' 15
    # Command rows render OMP-style with the $ prefix; the Output separator
    # sits between the command and the captured output.
    wait_for '$ printf' 15
    wait_for ' Output ' 15

    # --- Prompt 2 (same turn): assistant text with an unclosed code fence ---
    wait_for 'code · rust' 40
    wait_for 'fn main() {' 40
    wait_for '… (unclosed fence)' 40

    tmux capture-pane -p -S -3000 -t "$session":0 >"$evidence/tui.txt" 2>/dev/null || true
    assert_file_contains "$evidence/tui.txt" \
        '# QA shell probe' \
        '# second probe line' \
        'card-one' \
        'card-two' \
        ' Output ' \
        'code · rust' \
        'fn main() {' \
        '… (unclosed fence)'
    # The unclosed marker must sit on a frame bottom border (╰…unclosed fence…╯).
    grep -E '╰.*unclosed fence' "$evidence/tui.txt" >"$evidence/fence-bottom.txt" \
        || fail "user.$SCENARIO: unclosed fence marker not on a bottom border"

    tmux send-keys -t "$session":0 -l '/quit'
    tmux send-keys -t "$session":0 Enter
    if ! tmux_wait_for "$session" 12 '===TUI-DONE-bash-card-fence===' >"$evidence/quit.txt"; then
        log "user.$SCENARIO: quit marker not seen; scenario assertions already passed, tearing down"
    fi
    tmux kill-session -t "$session" 2>/dev/null || true

    printf 'user.%s passed\nevidence=%s\n' "$SCENARIO" "$evidence"
}

main "$@"
