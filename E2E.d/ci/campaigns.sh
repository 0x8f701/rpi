#!/usr/bin/env bash
set -euo pipefail
SCRIPT_DIR="$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
# shellcheck source=../lib/common.sh
. "$SCRIPT_DIR/../lib/common.sh"

list_scenarios() {
    printf '%s\n' \
        'campaign.version - isolated rpi --version' \
        'campaign.faux-json - offline faux JSON lifecycle' \
        'campaign.rpc-state - todo dependency, goal, loop, process, tools, and session RPC' \
        'campaign.orchestration - full Todo lifecycle + NL/IRC both ways + compact agents tmux' \
        'campaign.openai-schema - strict OpenAI todo/task schema request validation' \
        'campaign.bash-tui - foreground Bash stdin isolation and unattended TUI completion' \
        'campaign.workflow - opt-in multi-workflow RPC+tmux release gate' \
        'campaign.extension - trusted Bun command and chain in isolated TUI (requires bun, explicit group)' \
        'campaign.tmux-matrix - TUI visual/input capture at 90x31, 120x31, and 163x40 (explicit group)'
    "$SCRIPT_DIR/orchestration.sh" list
    "$SCRIPT_DIR/workflow.sh" list
}

run_version() {
    local root evidence
    root="$(scenario_workspace version)"; evidence="$EVIDENCE_ROOT/version"
    isolated_rpi_timeout 30 "$root/home" "$root/workspace" --version > "$evidence/version.log" 2>&1
}

run_faux_json() {
    local root evidence
    root="$(scenario_workspace faux-json)"; evidence="$EVIDENCE_ROOT/faux-json"
    isolated_rpi_timeout 30 "$root/home" "$root/workspace" --model faux/faux-1 --mode json 'deterministic smoke' > "$evidence/events.jsonl" 2> "$evidence/stderr.log"
    python3 "$E2E_DIR/lib/assert_jsonl.py" events "$evidence/events.jsonl" deterministic-e2e-reply
}

run_rpc_state() {
    local root evidence output
    root="$(scenario_workspace rpc-state)"; evidence="$EVIDENCE_ROOT/rpc-state"; output="$evidence/output.jsonl"
    run_with_timeout 40 python3 "$E2E_DIR/lib/run_rpc_campaign.py" \
        --rpi "$RPI_BIN" \
        --home "$root/home" \
        --workspace "$root/workspace" \
        --output "$output" \
        --stderr "$evidence/stderr.log"
    python3 "$E2E_DIR/lib/assert_jsonl.py" rpc "$output" models commands bash todos todo-state goal goal-get loop loops spawn process-list state name tree
    python3 - "$output" <<'PY'
import json, sys
rows=[json.loads(line) for line in open(sys.argv[1], encoding='utf-8') if line.strip()]
r={row.get('id'):row for row in rows if row.get('type')=='response'}
assert any(m.get('provider')=='faux' and m.get('id')=='faux-1' for m in r['models']['data']['models'])
assert r['goal-get']['data']['current']['objective']=='deterministic release readiness'
assert r['todo-state']['data']['todoPhases'][0]['tasks'][1]['blockedBy'][0]['taskId']=='inventory'
assert len(r['loops']['data'])==1
assert isinstance(r['commands']['data']['commands'], list)
assert r['bash']['data']['output']=='tools-campaign'
assert len(r['process-list']['data']) >= 1
PY
}

run_openai_schema() {
    local root evidence
    root="$(scenario_workspace openai-schema)"; evidence="$EVIDENCE_ROOT/openai-schema"
    run_with_timeout 90 python3 "$E2E_DIR/lib/run_openai_schema_campaign.py" \
        --rpi "$RPI_BIN" --home "$root/home" --workspace "$root/workspace" \
        --output "$evidence/output.log" --stderr "$evidence/stderr.log" \
        > "$evidence/assertions.json"
}

run_bash_tui() {
    require_cmd tmux
    require_cmd python3
    local root evidence
    root="$(scenario_workspace bash-tui)"; evidence="$EVIDENCE_ROOT/bash-tui"
    run_with_timeout 90 python3 "$E2E_DIR/lib/run_bash_tui_campaign.py" \
        --rpi "$RPI_BIN" --home "$root/home" --workspace "$root/workspace" --evidence "$evidence" \
        >"$evidence/result.json"
}

run_extension() {
    require_cmd bun; require_cmd tmux
    local root evidence extension session
    root="$(scenario_workspace extension)"; evidence="$EVIDENCE_ROOT/extension"; extension="$root/extension"; session="$(unique_tmux_name extension)"
    mkdir -p "$extension"
    cat > "$extension/pi-extension.json" <<'EOF'
{"schemaVersion":1,"id":"e2e-run","runtime":"bun","entry":"index.ts","capabilities":["commands","ui"],"uiCapabilities":["notify"]}
EOF
    cat > "$extension/index.ts" <<'EOF'
export default function (pi: any) {
  pi.registerCommand("alpha", {handler: async (args: string) => `alpha:${args || "none"}`});
  pi.registerCommand("beta", {handler: async (args: string) => `beta:${args || "none"}`});
}
EOF
    register_tmux_session "$session"
    tmux new-session -d -s "$session" -x 120 -y 31 -c "$root/workspace" \
        "env HOME='$root/home' USERPROFILE='$root/home' PI_CODING_AGENT_DIR='$root/home/.pi/agent' PI_OFFLINE=1 PI_SKIP_VERSION_CHECK=1 TERM=xterm-256color '$RPI_BIN' --offline --model faux/faux-1 --extension '$extension'"
    sleep 2
    tmux send-keys -t "$session":0 '/run alpha hello' C-m
    sleep 1
    tmux send-keys -t "$session":0 '/chain alpha one | beta two' C-m
    sleep 2
    tmux capture-pane -p -e -S -1000 -t "$session":0 > "$evidence/tui.ansi"
    tmux capture-pane -p -S -1000 -t "$session":0 > "$evidence/tui.txt"
    grep -F 'alpha:hello' "$evidence/tui.txt" >/dev/null
    grep -F 'beta:two' "$evidence/tui.txt" >/dev/null
}

run_tmux_matrix() {
    require_cmd tmux
    local size cols rows root evidence session
    for size in 90x31 120x31 163x40; do
        cols="${size%x*}"; rows="${size#*x}"; root="$(scenario_workspace "tmux-$size")"; evidence="$EVIDENCE_ROOT/tmux-$size"; session="$(unique_tmux_name "matrix-$size")"
        register_tmux_session "$session"
        tmux new-session -d -s "$session" -x "$cols" -y "$rows" -c "$root/workspace" \
            "env HOME='$root/home' USERPROFILE='$root/home' PI_CODING_AGENT_DIR='$root/home/.pi/agent' PI_OFFLINE=1 PI_SKIP_VERSION_CHECK=1 PI_FAUX_RESPONSE=matrix-$size TERM=xterm-256color '$RPI_BIN' --offline --model faux/faux-1"
        sleep 2
        tmux send-keys -t "$session":0 'matrix input probe' C-m
        sleep 2
        tmux capture-pane -p -e -S -1000 -t "$session":0 > "$evidence/tui.ansi"
        tmux capture-pane -p -S -1000 -t "$session":0 > "$evidence/tui.txt"
        printf 'size=%s\ncols=%s\nrows=%s\n' "$size" "$cols" "$rows" > "$evidence/metadata.txt"
        grep -F 'matrix input probe' "$evidence/tui.txt" >/dev/null
        tmux kill-session -t "$session"
    done
}

case "${1:-run}" in
    list|--list|--dry-run) list_scenarios ;;
    run)
        prepare_roots
        require_rpi
        require_cmd python3
        run_version
        run_faux_json
        run_rpc_state
        run_openai_schema
        if command -v tmux >/dev/null 2>&1; then run_bash_tui; else log "tmux not available; skipped Bash TUI ownership lane"; fi
        # Deterministic orchestration lane (rpc + rust; tmux when available).
        "$SCRIPT_DIR/orchestration.sh" run
        # Multi-workflow remains an explicit release lane because it includes
        # destructive git worktree integration/conflict fixtures.
        printf 'campaigns passed\nevidence=%s\n' "$EVIDENCE_ROOT"
        printf 'note: run the workflow release lane separately: bash E2E.d/ci/workflow.sh run\n'
        ;;
    openai-schema) prepare_roots; require_rpi; require_cmd python3; run_openai_schema; printf 'openai schema campaign passed\nevidence=%s\n' "$EVIDENCE_ROOT" ;;
    bash-tui) prepare_roots; require_rpi; run_bash_tui; printf 'bash TUI campaign passed\nevidence=%s\n' "$EVIDENCE_ROOT" ;;
    orchestration) "$SCRIPT_DIR/orchestration.sh" run ;;
    orchestration-rpc) "$SCRIPT_DIR/orchestration.sh" rpc ;;
    orchestration-rust) "$SCRIPT_DIR/orchestration.sh" rust ;;
    orchestration-tmux) "$SCRIPT_DIR/orchestration.sh" tmux ;;
    workflow) "$SCRIPT_DIR/workflow.sh" run ;;
    workflow-rpc) "$SCRIPT_DIR/workflow.sh" rpc ;;
    workflow-tmux) "$SCRIPT_DIR/workflow.sh" tmux ;;
    extension) prepare_roots; require_rpi; run_extension; printf 'extension campaign passed\nevidence=%s\n' "$EVIDENCE_ROOT" ;;
    tmux-matrix) prepare_roots; require_rpi; run_tmux_matrix; printf 'tmux matrix passed\nevidence=%s\n' "$EVIDENCE_ROOT" ;;
    *) fail "usage: $0 [run|list|--dry-run|openai-schema|bash-tui|orchestration|orchestration-rpc|orchestration-rust|orchestration-tmux|workflow|workflow-rpc|workflow-tmux|extension|tmux-matrix]" ;;
esac
