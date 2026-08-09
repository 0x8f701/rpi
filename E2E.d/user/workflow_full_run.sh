#!/usr/bin/env bash
# User-perspective workflow full run: create an isolated workflow, watch its
# supervisor plan (mock returns a real todo tool call that builds the DAG plus
# a bash call that commits a plan file inside the workflow worktree), inspect
# the workflow-owned Todo DAG, integrate the branch, and verify the completed
# workflow plus the real merge commit in the source repository.
#
# The mock provider serves the supervisor's planning turn (todo init + bash
# commit), worker turns (text), and any other request.
set -euo pipefail
SCRIPT_DIR="$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
E2E_DIR="$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd -P)"
# shellcheck source=../lib/common.sh
. "$E2E_DIR/lib/common.sh"
# shellcheck source=../lib/orchestration_fixtures.sh
. "$E2E_DIR/lib/orchestration_fixtures.sh"
# shellcheck source=../lib/workflow_fixtures.sh
. "$E2E_DIR/lib/workflow_fixtures.sh"

SCENARIO="workflow-full-run"

# Product API gate (same contract as E2E.d/ci/workflow.sh): fail closed when
# the workflow list RPC is unavailable rather than claiming false coverage.
require_workflow_apis_or_block() {
    local home="$1" workspace="$2" status_path="$3"
    if workflow_product_apis_available "$home" "$workspace"; then
        WORKFLOW_PRODUCT_APIS=present
        export WORKFLOW_PRODUCT_APIS
        write_workflow_execution_status "$status_path" "apis_present" "workflow_list probe succeeded"
        return 0
    fi
    WORKFLOW_PRODUCT_APIS=absent
    export WORKFLOW_PRODUCT_APIS
    write_workflow_execution_status "$status_path" "blocked_missing_product_apis" \
        "workflow_list RPC not successful; campaign registered but not executable"
    fail "user.$SCENARIO: workflow product APIs not landed (workflow_list probe failed); refusing false pass"
}

wait_for() { # needle [timeout]
    local needle="$1" timeout="${2:-40}"
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
    python3 "$E2E_DIR/lib/user_mock_server.py" --scenario workflow --port-file "$port_file" \
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
    require_cmd git

    local root evidence session port plan_log
    root="$(scenario_workspace "$SCENARIO")"
    evidence="$EVIDENCE_ROOT/$SCENARIO"
    session="$(unique_tmux_name "$SCENARIO")"
    register_tmux_session "$session"

    prepare_workflow_home "$root/home"
    prepare_workflow_git_workspace "$root/workspace"
    require_workflow_apis_or_block "$root/home" "$root/workspace" "$evidence/execution-status.txt"

    port="$(start_mock "$root")"
    cat >"$root/home/.pi/agent/models.json" <<EOF
{
  "providers": {
    "user-workflow": {
      "baseUrl": "http://127.0.0.1:$port",
      "api": "openai-completions",
      "models": [
        { "id": "mock", "name": "Workflow Mock", "contextWindow": 32768, "maxTokens": 2048 }
      ]
    }
  }
}
EOF

    tmux new-session -d -s "$session" -x 150 -y 44 -c "$root/workspace" \
        "env HOME='$root/home' USERPROFILE='$root/home' \
PI_CODING_AGENT_DIR='$root/home/.pi/agent' \
PI_OFFLINE=1 PI_SKIP_VERSION_CHECK=1 \
TERM=xterm-256color \
'$RPI_BIN' --offline --model user-workflow/mock --api-key user-mock-key; printf '===TUI-DONE-workflow-full-run===\n'"

    tmux_wait_for "$session" 30 'user-workflow/mock' >"$evidence/boot.txt" || true
    # Dismiss the startup resume selector when it is showing (harmless otherwise).
    tmux send-keys -t "$session":0 Escape
    sleep 0.3

    # --- Create: the supervisor plans (todo init + worktree commit), workers
    # execute, and the settled DAG auto-integrates the branch. ---
    send_cmd '/workflow create ship-flow ship the widget'
    wait_for 'ship-flow' 60
    # The workflow event projects the live status; the terminal state is
    # completed (auto-integrate after the DAG settles).
    wait_for 'Workflow ship-flow · completed' 90

    # --- Git side effects: the planning commit landed on the source branch ---
    plan_log="$(git -C "$root/workspace" log --oneline 2>/dev/null || true)"
    printf '%s\n' "$plan_log" >"$evidence/git-log.txt"
    grep -F -- 'e2e plan' "$evidence/git-log.txt" >/dev/null \
        || fail "user.$SCENARIO: planning commit 'e2e plan' missing from source git log"
    [ -f "$root/workspace/PLAN.e2e" ] \
        || fail "user.$SCENARIO: PLAN.e2e missing from integrated source worktree"

    # --- Workflows page: the row reads completed with the integration applied ---
    send_cmd '/workflow'
    wait_for ' Workflows ·' 20
    wait_for 'ship-flow' 20
    wait_for 'completed' 20

    # --- Detail page: Status + Todo DAG + Integration sections ---
    tmux send-keys -t "$session":0 Enter
    wait_for 'Todo DAG' 30
    wait_for 'compile widget' 30
    wait_for 'ship widget' 30
    wait_for 'Status' 15
    wait_for 'Integrated' 30

    # Close the panel.
    tmux send-keys -t "$session":0 Escape
    sleep 0.4
    tmux send-keys -t "$session":0 Escape
    sleep 0.4

    tmux capture-pane -p -S -3000 -t "$session":0 >"$evidence/tui.txt" 2>/dev/null || true
    tmux send-keys -t "$session":0 -l '/quit'
    tmux send-keys -t "$session":0 Enter
    if ! tmux_wait_for "$session" 12 '===TUI-DONE-workflow-full-run===' >"$evidence/quit.txt"; then
        log "user.$SCENARIO: quit marker not seen; scenario assertions already passed, tearing down"
    fi
    tmux kill-session -t "$session" 2>/dev/null || true

    # Panel content is repainted, never scrolled; the per-step waits plus the
    # git side-effect checks above are the assertions.
    write_workflow_execution_status "$evidence/execution-status.txt" "passed" \
        "workflow full run tmux checks green"
    printf 'user.%s passed\nevidence=%s\n' "$SCENARIO" "$evidence"
}

main "$@"
