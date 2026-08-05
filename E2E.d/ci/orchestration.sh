#!/usr/bin/env bash
# Deterministic orchestration + TUI campaign lane.
# Covers NL routing, Todo DAG coordinator execution, Goal details,
# bidirectional IRC (rust authoritative), supervised /ps, exact /goal,
# Subagents-only layout (hard), and image placeholder (rust fixture).
set -euo pipefail
SCRIPT_DIR="$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
# shellcheck source=../lib/common.sh
. "$SCRIPT_DIR/../lib/common.sh"
# shellcheck source=../lib/orchestration_fixtures.sh
. "$SCRIPT_DIR/../lib/orchestration_fixtures.sh"

# Release gates require the pinned 1.88.0 toolchain. Never bare `cargo test`.
CARGO_E2E=(cargo +1.88.0)

list_scenarios() {
    printf '%s\n' \
        'orchestration.rpc - goal details, todo readiness/blocked projection, supervised process RPC' \
        'orchestration.rust - cargo +1.88.0 existing todo_dag_execution + NL/IRC/image/ps' \
        'orchestration.tmux - hard Subagents-only + Goal//ps//goal exact; separate Todo layout' \
        'orchestration.run - rpc + rust + tmux umbrella'
}

run_orchestration_rpc() {
    local root evidence
    require_rpi
    require_cmd python3
    root="$(scenario_workspace orchestration-rpc)"
    evidence="$EVIDENCE_ROOT/orchestration-rpc"
    prepare_orchestration_home "$root/home"
    run_with_timeout 90 python3 "$E2E_DIR/lib/run_orchestration_campaign.py" \
        --rpi "$RPI_BIN" \
        --home "$root/home" \
        --workspace "$root/workspace" \
        --output "$evidence/output.jsonl" \
        --stderr "$evidence/stderr.log" \
        --evidence "$evidence" \
        --timeout 35
    python3 - "$evidence/summary.json" <<'PY'
import json, sys
from pathlib import Path
summary = json.loads(Path(sys.argv[1]).read_text(encoding="utf-8"))
required = {
    "goal-details",
    "todo-ready-roots-and-blocked-join",
    "todo-exact-task-ids",
    "todo-exact-task-id-completion",
    "todo-dependent-after-roots",
    "todo-open-work-remains-after-partial",
    "todo-open-work-remains",
    "todo-blocked-only-projection",
    "todo-all-terminal-no-ready",
    "process-list-contains-supervised",
    "process-stopped-and-cleaned",
    "nl-prompts-issued",
}
missing = sorted(required - set(summary.get("checks") or []))
if missing:
    raise SystemExit(f"orchestration rpc missing checks: {missing}")
goal = summary.get("goal") or {}
for key in ("objective", "lifecycle", "tokensUsed", "tokenBudget", "activeTimeSeconds"):
    if key not in goal:
        raise SystemExit(f"goal summary missing {key}: {goal!r}")
if goal["objective"] != "deterministic orchestration readiness":
    raise SystemExit(goal)
if goal["lifecycle"] != "active":
    raise SystemExit(goal)
if goal["tokensUsed"] != 42 or goal["tokenBudget"] != 1000 or goal["activeTimeSeconds"] != 7:
    raise SystemExit(goal)
todo = summary.get("todoInitial") or {}
if not (todo.get("rootAReady") and todo.get("rootBReady") and not todo.get("joinReady")):
    raise SystemExit(f"todo initial readiness contract failed: {todo!r}")
if set(todo.get("joinBlockedBy") or []) != {"root-a", "root-b"}:
    raise SystemExit(f"todo blockedBy contract failed: {todo!r}")
if set(todo.get("exactIds") or []) != {"join", "root-a", "root-b"}:
    raise SystemExit(f"todo exact ids contract failed: {todo!r}")
after = summary.get("todoAfterRoots") or {}
if after.get("completed") != 2 or not after.get("joinReady"):
    raise SystemExit(f"todo after-roots contract failed: {after!r}")
if after.get("joinStatus") == "completed":
    raise SystemExit(f"join must stay open after roots: {after!r}")
terminal = summary.get("todoTerminal") or {}
if set(terminal.get("ids") or []) != {"done-a", "done-b"}:
    raise SystemExit(f"todo terminal ids failed: {terminal!r}")
print("orchestration.rpc assertions passed")
print("note: coordinator execution is authoritative in orchestration.rust todo_dag_execution")
print("note: rust surface post-FF0F = two_ready_roots_overlap + failed_and_cancelled_owners_stay_open")
print("note: restore/transition/abort filters removed until product re-lands those tests")
PY
    log "orchestration.rpc evidence=$evidence"
}

run_orchestration_rust() {
    local evidence logf
    require_cmd cargo
    evidence="$EVIDENCE_ROOT/orchestration-rust"
    mkdir -p "$evidence"
    logf="$evidence/cargo.log"

    {
        printf 'scenario=orchestration.rust\n'
        printf 'toolchain=1.88.0\n'
        # Source-backed contracts only (post-FF0F todo_dag_execution surface).
        printf 'todo_lifecycle_contracts=ready_roots_overlap,todo_task_id_completion,failed_cancelled_open\n'

        # Exact NL agent mention → Subagents job cards; skill-only does not spawn.
        (cd "$REPO_ROOT" && run_with_timeout 300 "${CARGO_E2E[@]}" test -p pi-cli --test nl_exact_agent_spawn --locked -- --nocapture)

        # Routing contracts: Have researcher vs Use research.
        (cd "$REPO_ROOT" && run_with_timeout 300 "${CARGO_E2E[@]}" test -p pi-coding --test routing_contracts --locked -- --nocapture)

        # Todo tool DAG lifecycle (readiness / blockedBy projection).
        (cd "$REPO_ROOT" && run_with_timeout 300 "${CARGO_E2E[@]}" test -p pi-coding --test todo_dag_lifecycle --locked -- --nocapture)

        # ---- Authoritative Todo DAG coordinator (existing tests only; fail closed) ----
        # Full binary first so additions are never silently dropped from CI.
        (cd "$REPO_ROOT" && run_with_timeout 360 "${CARGO_E2E[@]}" test -p pi-coding --test todo_dag_execution --locked -- --nocapture)

        # 1) Multiple ready roots overlap; join waits; exact todoTaskId ownership/completion.
        (cd "$REPO_ROOT" && run_with_timeout 300 "${CARGO_E2E[@]}" test -p pi-coding --test todo_dag_execution \
            two_ready_roots_overlap_and_join_waits_for_both_before_three_of_three --locked -- --nocapture)

        # 2) Failed/cancelled owners leave tasks open; reconcile idempotent; Blocked status.
        (cd "$REPO_ROOT" && run_with_timeout 300 "${CARGO_E2E[@]}" test -p pi-coding --test todo_dag_execution \
            failed_and_cancelled_owners_stay_open_and_terminal_reconciliation_is_idempotent --locked -- --nocapture)

        # Goal details formatter (Objective/Status/Tokens/Time).
        (cd "$REPO_ROOT" && run_with_timeout 240 "${CARGO_E2E[@]}" test -p pi-cli formats_empty_goal_details_for_overlay --locked -- --nocapture)

        # IRC presentation both directions: named sender→recipient, separate body,
        # no raw XML (human renderer + TUI MessageDelivered path).
        (cd "$REPO_ROOT" && run_with_timeout 240 "${CARGO_E2E[@]}" test -p pi-cli orchestration_irc_renders_named_label_body_reply --locked -- --nocapture)
        (cd "$REPO_ROOT" && run_with_timeout 240 "${CARGO_E2E[@]}" test -p pi-cli message_delivered_event_renders_once_and_dedupes_custom_message --locked -- --nocapture)

        # Runtime bidirectional IRC while supervised (Main↔child / peer).
        (cd "$REPO_ROOT" && run_with_timeout 300 "${CARGO_E2E[@]}" test -p pi-coding --test orchestration_supervision main_supervises_two_children_with_irc --locked -- --nocapture)

        # Authoritative real task + identity-bound child hub AgentTool chain.
        (cd "$REPO_ROOT" && run_with_timeout 300 "${CARGO_E2E[@]}" test -p pi-coding --test orchestration \
            real_task_children_route_main_alpha_beta_main_through_owned_hub_tools --locked -- --nocapture)

        # Image placeholder [Image #N, WIDTHxHEIGHT].
        (cd "$REPO_ROOT" && run_with_timeout 240 "${CARGO_E2E[@]}" test -p pi-cli clipboard_png_fixture_attaches_one_image --locked -- --nocapture)

        # Stale Todo panel after tree/fork/keyboard-new: replace_transcript must
        # refresh todo_phases from the application (not leave prior DAG chrome).
        (cd "$REPO_ROOT" && run_with_timeout 240 "${CARGO_E2E[@]}" test -p pi-cli replace_transcript_refreshes_todo_phases_from_application --locked -- --nocapture)

        # Supervised process /ps PTY (unix-only module).
        (cd "$REPO_ROOT" && run_with_timeout 300 "${CARGO_E2E[@]}" test -p pi-cli --test process_ps_pty --locked -- --nocapture)
    } >"$logf" 2>&1 || {
        tail -n 200 "$logf" >&2 || true
        fail "orchestration.rust cargo +1.88.0 gates failed (see $logf)"
    }

    # Fail closed: only EXISTING todo_dag_execution test names (post-FF0F surface).
    assert_file_contains "$logf" "todo_dag_execution"
    assert_file_contains "$logf" "two_ready_roots_overlap_and_join_waits_for_both_before_three_of_three"
    assert_file_contains "$logf" "failed_and_cancelled_owners_stay_open_and_terminal_reconciliation_is_idempotent"
    assert_file_contains "$logf" "nl_exact_agent_spawn"
    assert_file_contains "$logf" "main_supervises_two_children_with_irc"
    assert_file_contains "$logf" "real_task_children_route_main_alpha_beta_main_through_owned_hub_tools"
    assert_file_contains "$logf" "message_delivered_event_renders_once"
    assert_file_contains "$logf" "replace_transcript_refreshes_todo_phases"
    printf 'orchestration.rust passed\nlog=%s\n' "$logf"
}

# Tmux-backed captures. Subagents-only assertion is HARD with empty Todos.
run_orchestration_tmux() {
    require_rpi
    require_cmd tmux
    require_cmd python3

    local root evidence session
    root="$(scenario_workspace orchestration-tmux)"
    evidence="$EVIDENCE_ROOT/orchestration-tmux"
    session="$(unique_tmux_name orchestration)"
    prepare_orchestration_home "$root/home"
    register_tmux_session "$session"

    printf 'orchestration-workspace\n' >"$root/workspace/README.e2e"

    tmux new-session -d -s "$session" -x 120 -y 40 -c "$root/workspace" \
        "env HOME='$root/home' USERPROFILE='$root/home' \
PI_CODING_AGENT_DIR='$root/home/.pi/agent' \
PI_OFFLINE=1 PI_SKIP_VERSION_CHECK=1 \
PI_FAUX_RESPONSE='deterministic-orchestration-reply' \
TERM=xterm-256color \
'$RPI_BIN' --offline --model faux/faux-1"

    # First paint.
    tmux_wait_for "$session" 20 'Ready' >"$evidence/boot.txt" || true
    sleep 1

    # --- Exact /goal input (no duplication) ---
    tmux send-keys -t "$session":0 '/'
    sleep 0.05
    tmux send-keys -t "$session":0 'g'
    sleep 0.05
    tmux send-keys -t "$session":0 'o'
    sleep 0.05
    tmux send-keys -t "$session":0 'a'
    sleep 0.05
    tmux send-keys -t "$session":0 'l'
    sleep 0.25
    tmux capture-pane -p -S -300 -t "$session":0 >"$evidence/goal-typed.txt"
    assert_file_contains "$evidence/goal-typed.txt" '/goal'
    assert_file_lacks "$evidence/goal-typed.txt" '/goall'
    tmux send-keys -t "$session":0 Enter
    sleep 1
    tmux capture-pane -p -S -500 -t "$session":0 >"$evidence/goal-panel.txt"
    if ! grep -E -e 'Goal' -e 'Show details' -e 'no goal' -e 'Create goal' "$evidence/goal-panel.txt" >/dev/null; then
        fail "bare /goal did not open goal UI"
    fi
    tmux send-keys -t "$session":0 Escape
    sleep 0.3

    # Create goal then inspect details (objective/status/tokens/time).
    tmux send-keys -t "$session":0 -l '/goal create --tokens 500 deterministic orchestration readiness'
    tmux send-keys -t "$session":0 Enter
    sleep 1
    tmux send-keys -t "$session":0 -l '/goal'
    tmux send-keys -t "$session":0 Enter
    sleep 0.8
    tmux send-keys -t "$session":0 Enter
    sleep 0.8
    tmux capture-pane -p -S -800 -t "$session":0 >"$evidence/goal-details.txt"
    if ! grep -E -e 'Objective:' -e 'deterministic orchestration readiness' -e 'active' "$evidence/goal-details.txt" >/dev/null; then
        fail "goal details missing objective/status evidence"
    fi
    tmux send-keys -t "$session":0 Escape
    sleep 0.2
    assert_tmux_composer_editable "$session" "$evidence/goal-composer.txt" 'GOAL-COMPOSER-SENTINEL'


    # --- Subagents-only HARD path: Todos empty at assertion ---
    # Leave todos empty (default session). Do not load a todo list before spawn.
    tmux capture-pane -p -S -400 -t "$session":0 >"$evidence/pre-subagents.txt"
    if grep -E -e 'Todos ·' "$evidence/pre-subagents.txt" >/dev/null; then
        # Best-effort clear via empty todo command if chrome somehow present.
        tmux send-keys -t "$session":0 -l '/todo'
        tmux send-keys -t "$session":0 Enter
        sleep 0.5
    fi

    tmux send-keys -t "$session":0 -l 'Have researcher study this'
    tmux send-keys -t "$session":0 Enter
    if ! tmux_wait_for "$session" 25 'Task 1 agents' 'researcher' >"$evidence/subagents-exact.txt"; then
        tmux capture-pane -p -S -2000 -t "$session":0 >"$evidence/subagents-exact.txt"
        fail "HARD: compact agent card with researcher not observed after exact NL spawn"
    fi
    # Require a visible lifecycle token.
    if ! grep -E -ie 'queued|running|completed|parked|idle' "$evidence/subagents-exact.txt" >/dev/null; then
        fail "HARD: Subagents researcher row missing lifecycle state"
    fi
    # HARD: no Todo panel chrome during Subagents-only assertion.
    if grep -E -e 'Todos ·' "$evidence/subagents-exact.txt" >/dev/null; then
        fail "HARD: Todos chrome present during Subagents-only assertion"
    fi
    log "Subagents-only hard assertion passed"

    local before_count after_count
    before_count="$(grep -c -E 'researcher' "$evidence/subagents-exact.txt" || true)"
    tmux send-keys -t "$session":0 -l 'Use research for this'
    tmux send-keys -t "$session":0 Enter
    sleep 2
    tmux capture-pane -p -S -1500 -t "$session":0 >"$evidence/subagents-skill-only.txt"
    after_count="$(grep -c -E 'researcher' "$evidence/subagents-skill-only.txt" || true)"
    if [ "${after_count:-0}" -gt $(( ${before_count:-0} + 1 )) ]; then
        fail "skill-only prompt appears to have spawned extra researcher jobs ($before_count -> $after_count)"
    fi

    # --- Real rpi/tmux child-owned hub AgentTool chain ---
    run_with_timeout 55 python3 "$E2E_DIR/lib/run_hub_tui_campaign.py" \
        --rpi "$RPI_BIN" \
        --home "$root/hub-home" \
        --workspace "$root/hub-workspace" \
        --evidence "$evidence/hub-tui" \
        >"$evidence/hub-tui-result.json"
    assert_file_contains "$evidence/hub-tui/tui.txt" \
        'IRC · Beta → Main' 'beta-to-main-tmux' \
        'Alpha (Alpha) · completed' 'Beta (Beta) · completed'
    assert_file_lacks "$evidence/hub-tui/tui.txt" '<orchestration-message'
    assert_file_contains "$evidence/hub-tui/assertions.json" \
        'alpha-child-owned-hub-wait-send' 'beta-child-owned-hub-wait-send'

    # --- Dense Todo pressure at the reported 120x40 size ---
    run_with_timeout 45 python3 "$E2E_DIR/lib/run_todo_tui_campaign.py" \
        --rpi "$RPI_BIN" \
        --home "$root/todo-home" \
        --workspace "$root/todo-workspace" \
        --evidence "$evidence/todo-tui" \
        >"$evidence/todo-tui-result.json"
    assert_file_contains "$evidence/todo-tui/tui.txt" \
        'Todos · 7 active' 'TODO-COMPOSER-SENTINELX'
    grep -E -e 'more (open|active) todos' "$evidence/todo-tui/tui.txt" >/dev/null \
        || fail "dense Todo projection did not expose a compact overflow marker"


    # Supervised background server via /process + exact /ps.
    tmux send-keys -t "$session":0 Escape
    sleep 0.2
    tmux send-keys -t "$session":0 -l "/process start sh -c 'printf orchestration-server-ready; sleep 90'"
    tmux send-keys -t "$session":0 Enter
    sleep 1.2
    tmux send-keys -t "$session":0 '/'
    sleep 0.05
    tmux send-keys -t "$session":0 'p'
    sleep 0.05
    tmux send-keys -t "$session":0 's'
    sleep 0.25
    tmux capture-pane -p -S -200 -t "$session":0 >"$evidence/ps-typed.txt"
    assert_file_contains "$evidence/ps-typed.txt" '/ps'
    tmux send-keys -t "$session":0 Enter
    sleep 1
    tmux capture-pane -p -S -800 -t "$session":0 >"$evidence/ps-panel.txt"
    if ! grep -E -e 'Processes' -e 'sleep' -e 'running' -e 'Running' -e 'orchestration-server' -e 'sh -c' "$evidence/ps-panel.txt" >/dev/null; then
        fail "/ps did not show supervised process"
    fi
    tmux send-keys -t "$session":0 Escape
    sleep 0.2

    tmux capture-pane -p -e -S -2000 -t "$session":0 >"$evidence/tui.ansi"
    tmux capture-pane -p -S -2000 -t "$session":0 >"$evidence/tui.txt"
    assert_file_lacks "$evidence/tui.txt" '<orchestration-message'
    if ! grep -E -e 'Goal' -e 'deterministic orchestration readiness' "$evidence/tui.txt" >/dev/null; then
        fail "final TUI capture missing Goal markers"
    fi
    if ! grep -E -e 'Task [0-9]+ agents' -e 'Processes' "$evidence/tui.txt" >/dev/null; then
        fail "final TUI capture missing compact agent/process layout markers"
    fi

    # Bidirectional IRC bodies/labels are authoritative in rust gates
    # (orchestration_supervision + message_delivered + orchestration_irc_*).
    # Tmux only forbids leaking raw orchestration XML into the human layout.
    printf 'irc_tmux=raw_xml_forbidden\nirc_authoritative=rust:orchestration_supervision+message_delivered+orchestration_irc\n' \
        >"$evidence/irc-meta.txt"

    if command -v xclip >/dev/null 2>&1 && { [ -n "${DISPLAY:-}" ] || [ -n "${WAYLAND_DISPLAY:-}" ]; }; then
        write_orchestration_png_fixture "$root/workspace/clip.png"
        xclip -selection clipboard -t image/png -i "$root/workspace/clip.png" 2>/dev/null || true
        printf 'clipboard_png=attempted\n' >"$evidence/clipboard-meta.txt"
    else
        printf 'clipboard_png=skipped\nreason=no-xclip-or-display\n' >"$evidence/clipboard-meta.txt"
        log "clipboard image injection skipped; rust clipboard_png_fixture is authoritative"
    fi

    tmux kill-session -t "$session" 2>/dev/null || true
    printf 'orchestration.tmux passed\nevidence=%s\n' "$evidence"
}

run_all() {
    prepare_roots
    run_orchestration_rpc
    run_orchestration_rust
    if command -v tmux >/dev/null 2>&1; then
        run_orchestration_tmux
    else
        log "tmux not available; skipped orchestration.tmux (rpc+rust still ran)"
    fi
    printf 'orchestration campaigns passed\nevidence=%s\n' "$EVIDENCE_ROOT"
}

case "${1:-list}" in
    list|--list|--dry-run) list_scenarios ;;
    rpc) prepare_roots; run_orchestration_rpc; printf 'orchestration.rpc passed\nevidence=%s\n' "$EVIDENCE_ROOT" ;;
    rust) prepare_roots; run_orchestration_rust ;;
    tmux) prepare_roots; run_orchestration_tmux ;;
    run) run_all ;;
    *) fail "usage: $0 [list|--dry-run|run|rpc|rust|tmux]" ;;
esac
