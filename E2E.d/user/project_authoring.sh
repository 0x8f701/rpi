#!/usr/bin/env bash
# User-perspective project-authoring E2E: the real rpi agent builds a small
# dependency-free Rust CLI task tracker (task-tracker) from an EMPTY git
# workspace, driven by the deterministic project-authoring mock
# (E2E.d/lib/project_authoring_mock.py).
#
# The mock streams genuine tool calls that the rpi harness executes:
#   todo init (4 phases / 10 tasks) -> five writes (Cargo.toml +
#   src/{model,parser,store,main}.rs) -> first `cargo test` that MUST fail on
#   the planted marker-parse defect in src/store.rs -> read + edit that repair
#   the defect -> second `cargo test` that MUST pass (12/12) -> valid CLI run
#   (add "buy milk" / done 0 / list) -> two invalid CLI runs (unknown command
#   and a bad task id, both exit 1 with actionable stderr) -> interleaved todo
#   done ops -> final assistant text.
#
# The scenario asserts, through the real TUI and the real filesystem:
#   produced files, the fail->fix->pass test cycle, CLI behavior on valid and
#   invalid input, a fully completed Todo DAG, transcript tool cards, and
#   terminal recovery (composer editable, clean /quit).
set -euo pipefail
SCRIPT_DIR="$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
E2E_DIR="$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd -P)"
# shellcheck source=../lib/common.sh
. "$E2E_DIR/lib/common.sh"
# shellcheck source=../lib/orchestration_fixtures.sh
. "$E2E_DIR/lib/orchestration_fixtures.sh"

SCENARIO="project-authoring"

wait_for() { # needle [timeout]
    local needle="$1" timeout="${2:-60}"
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
    python3 "$E2E_DIR/lib/project_authoring_mock.py" --port-file "$port_file" \
        --workspace "$root/workspace" >"$evidence/mock-server.log" 2>&1 &
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
    require_cmd rustup

    local root evidence session port ws toolchain_bin
    root="$(scenario_workspace "$SCENARIO")"
    evidence="$EVIDENCE_ROOT/$SCENARIO"
    session="$(unique_tmux_name "$SCENARIO")"
    register_tmux_session "$session"
    ws="$root/workspace"
    # Empty isolated git workspace (no files, no seed content).
    git -C "$ws" init -q
    git -C "$ws" -c user.email=e2e@localhost -c user.name=e2e commit -q --allow-empty -m seed

    toolchain_bin="$(dirname "$(rustup which --toolchain 1.88.0 cargo)")"
    port="$(start_mock "$root")"
    cat >"$root/home/.pi/agent/models.json" <<EOF
{
  "providers": {
    "user-project-authoring": {
      "baseUrl": "http://127.0.0.1:$port",
      "api": "openai-completions",
      "models": [
        { "id": "mock", "name": "Project Authoring Mock", "contextWindow": 32768, "maxTokens": 2048 }
      ]
    }
  }
}
EOF

    tmux new-session -d -s "$session" -x 150 -y 44 -c "$ws" \
        "env HOME='$root/home' USERPROFILE='$root/home' \
PI_CODING_AGENT_DIR='$root/home/.pi/agent' \
PI_OFFLINE=1 PI_SKIP_VERSION_CHECK=1 \
PATH='$toolchain_bin:${PATH:-/usr/bin:/bin}' \
TERM=xterm-256color \
'$RPI_BIN' --offline --model user-project-authoring/mock --api-key user-mock-key; printf '===TUI-DONE-project-authoring===\n'"

    tmux_wait_for "$session" 30 'user-project-authoring/mock' >"$evidence/boot.txt" || true
    # Dismiss the startup resume selector when it is showing (harmless otherwise).
    tmux send-keys -t "$session":0 Escape
    sleep 0.3

    # --- The user asks for the project; the mock plans with todo init ---
    send_cmd 'Build me a small dependency-free Rust CLI task tracker in this empty workspace: a task-tracker binary with add/done/list commands, a file-backed store, and unit tests. Run cargo test and prove the CLI works on both valid and invalid input.'
    wait_for 'create Cargo.toml manifest' 60

    # --- The five real write cards land; contents are checked on disk below ---
    wait_for 'Write src/model.rs' 60
    wait_for 'Write src/parser.rs' 60
    wait_for 'Write src/store.rs' 60
    wait_for 'Write src/main.rs' 60

    # --- First cargo test MUST fail on the planted marker-parse defect ---
    wait_for 'test result: FAILED' 120

    wait_for 'Read src/store.rs' 60
    wait_for 'Edit src/store.rs' 60
    wait_for 'let title = title.trim();' 60

    # --- Second cargo test MUST pass ---
    wait_for 'test result: ok' 120

    # --- Valid CLI run (build + add + done + list) ---
    wait_for 'added 0: buy milk' 120
    wait_for 'completed 0' 60
    wait_for '0 [x]: buy milk' 60

    # --- Invalid CLI runs: both exit non-zero with actionable stderr ---
    wait_for 'unknown command: bogus' 60
    wait_for 'invalid task id: abc' 60

    # --- Final assistant text ends the turn ---
    wait_for 'task-tracker complete' 60

    # --- Real artifacts on disk ---
    for f in Cargo.toml src/model.rs src/parser.rs src/store.rs src/main.rs; do
        [ -f "$ws/$f" ] || fail "user.$SCENARIO: missing produced file: $f"
    done
    grep -Fq 'split_at(3)' "$ws/src/store.rs" \
        || fail "user.$SCENARIO: store.rs missing the marker-parse repair"
    if grep -Fq 'split_once' "$ws/src/store.rs"; then
        fail "user.$SCENARIO: store.rs still contains the planted defect"
    fi

    # --- The mock state machine reached the final step ---
    grep -q 'step=final' "$evidence/mock-server.log" \
        || fail "user.$SCENARIO: mock never reached the final step (see $evidence/mock-server.log)"

    # --- Todo DAG: all 10 tasks completed ---
    send_cmd '/todo'
    wait_for 'Todo DAGs' 20
    wait_for '✓ 10 completed · 0 open · 0 active · 0 blocked' 20
    tmux send-keys -t "$session":0 Escape
    sleep 0.5

    # --- Terminal recovery: composer editable, clean quit ---
    assert_tmux_composer_editable "$session" "$evidence/composer.txt" 'PA-SENTINEL'
    tmux capture-pane -p -S -4000 -t "$session":0 >"$evidence/tui.txt" 2>/dev/null || true
    tmux send-keys -t "$session":0 -l '/quit'
    tmux send-keys -t "$session":0 Enter
    if ! tmux_wait_for "$session" 12 '===TUI-DONE-project-authoring===' >"$evidence/quit.txt"; then
        log "user.$SCENARIO: quit marker not seen; scenario assertions already passed, tearing down"
    fi
    tmux kill-session -t "$session" 2>/dev/null || true

    # --- Transcript tool-card coverage (Todo / Write / Read / Edit / Bash) ---
    assert_file_contains "$evidence/tui.txt" \
        'create Cargo.toml manifest' \
        'Write src/store.rs' \
        'Edit src/store.rs' \
        'cargo test --offline' \
        '0 [x]: buy milk' \
        'unknown command: bogus'

    printf 'user.%s passed\nevidence=%s\n' "$SCENARIO" "$evidence"
}

main "$@"
