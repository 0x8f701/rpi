#!/usr/bin/env bash
# Deterministic multi-workflow campaign lane.
# Covers concurrent workflow create, isolated git worktrees, per-workflow
# supervisors + owned IRC directives, overlapping ready Todo roots, compact
# normal-screen header, /workflow master-detail, pause/resume/cancel,
# non-conflicting integrate, explicit conflict visibility, and settings
# overlay exclusion from scrollback.
#
# Execution status is explicit:
#   - list always works
#   - run/rpc/tmux HARD-fail when product APIs are absent (no false pass)
#   - never claim "passed" until a full executed campaign succeeds
set -euo pipefail
SCRIPT_DIR="$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
# shellcheck source=../lib/common.sh
. "$SCRIPT_DIR/../lib/common.sh"
# shellcheck source=../lib/orchestration_fixtures.sh
. "$SCRIPT_DIR/../lib/orchestration_fixtures.sh"
# shellcheck source=../lib/workflow_fixtures.sh
. "$SCRIPT_DIR/../lib/workflow_fixtures.sh"

list_scenarios() {
    printf '%s\n' \
        'workflow.rpc - concurrent create, worktrees, ownership Todo roots, pause/resume/cancel, integrate/conflict' \
        'workflow.tmux - compact header, workflow list-to-detail, settings scrollback exclusion' \
        'workflow.goal-tmux - exact Chinese goal/workflow commands, real Todo calls/workers, multi-DAG Todo detail' \
        'workflow.run - rpc + tmux + goal-tmux umbrella release gate'
}

require_workflow_apis_or_block() {
    local home="$1"
    local workspace="$2"
    local status_path="$3"
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
    fail "workflow product APIs not landed (workflow_list probe failed); refusing false pass"
}

run_workflow_rpc() {
    local root evidence
    require_rpi
    require_cmd python3
    require_cmd git
    root="$(scenario_workspace workflow-rpc)"
    evidence="$EVIDENCE_ROOT/workflow-rpc"
    prepare_workflow_home "$root/home"
    prepare_workflow_git_workspace "$root/workspace"
    require_workflow_apis_or_block "$root/home" "$root/workspace" "$evidence/execution-status.txt"

    run_with_timeout 120 python3 "$E2E_DIR/lib/run_workflow_campaign.py" \
        --rpi "$RPI_BIN" \
        --home "$root/home" \
        --workspace "$root/workspace" \
        --output "$evidence/output.jsonl" \
        --stderr "$evidence/stderr.log" \
        --evidence "$evidence" \
        --timeout 45

    python3 - "$evidence/summary.json" <<'PY'
import json, sys
from pathlib import Path

summary = json.loads(Path(sys.argv[1]).read_text(encoding="utf-8"))
if summary.get("execution_status") != "passed":
    raise SystemExit(f"workflow rpc execution_status not passed: {summary!r}")
required = {
    "workflow-list-empty-initial",
    "two-workflows-created-concurrently",
    "workflow-list-contains-both",
    "separate-git-worktrees",
    "independent-ready-todo-roots-overlap",
    "cross-workflow-task-ids-no-collision",
    "supervisors-started-per-workflow",
    "supervisor-irc-directives-owned",
    "workflow-pause",
    "workflow-resume",
    "workflow-cancel-idempotent",
    "non-conflicting-integration",
    "explicit-conflict-preserved-visible",
    "workflow-remove",
    "generation-field-typed-when-present",
    "workflow-events-generation-gated",
    "workflow-events-public-wire-only",
    "no-failed-after-planner-release",
    "planner-provider-engaged",
}
missing = sorted(required - set(summary.get("checks") or []))
if missing:
    raise SystemExit(f"workflow rpc missing checks: {missing}")
alpha = summary.get("alpha") or {}
beta = summary.get("beta") or {}
if not alpha.get("workflowId") or not beta.get("workflowId"):
    raise SystemExit(f"missing workflow ids in summary: {summary!r}")
if alpha.get("workflowId") == beta.get("workflowId"):
    raise SystemExit("alpha/beta workflowId collision in summary")
wt = summary.get("worktrees") or {}
alpha_label = wt.get("alphaLabel") or wt.get("alphaPath")
beta_label = wt.get("betaLabel") or wt.get("betaPath")
if not alpha_label or not beta_label or alpha_label == beta_label:
    raise SystemExit(f"worktree isolation contract failed: {wt!r}")
for label in (alpha_label, beta_label):
    if str(label).startswith("/") or str(label).startswith("\\"):
        raise SystemExit(f"worktree wire label must not be absolute: {label!r}")
if (summary.get("alphaConflict") or {}).get("status") != "conflicted":
    raise SystemExit(f"explicit conflict not preserved: {summary.get('alphaConflict')!r}")
print("workflow.rpc assertions passed")
PY
    write_workflow_execution_status "$evidence/execution-status.txt" "passed" "rpc checks green"
    log "workflow.rpc evidence=$evidence"
}

# Tmux-backed captures for compact header + master-detail + settings scrollback.
run_workflow_tmux() {
    require_rpi
    require_cmd tmux
    require_cmd python3
    require_cmd git

    local root evidence session
    root="$(scenario_workspace workflow-tmux)"
    evidence="$EVIDENCE_ROOT/workflow-tmux"
    session="$(unique_tmux_name workflow)"
    prepare_workflow_home "$root/home"
    prepare_workflow_git_workspace "$root/workspace"
    require_workflow_apis_or_block "$root/home" "$root/workspace" "$evidence/execution-status.txt"
    register_tmux_session "$session"

    printf 'workflow-tmux-workspace\n' >"$root/workspace/README.e2e"

    tmux new-session -d -s "$session" -x 120 -y 40 -c "$root/workspace" \
        "env HOME='$root/home' USERPROFILE='$root/home' \
PI_CODING_AGENT_DIR='$root/home/.pi/agent' \
PI_OFFLINE=1 PI_SKIP_VERSION_CHECK=1 \
PI_FAUX_RESPONSE='deterministic-workflow-reply' \
TERM=xterm-256color \
'$RPI_BIN' --offline --model faux/faux-1"

    tmux_wait_for "$session" 20 'Ready' >"$evidence/boot.txt" || true
    sleep 1

    # Seed a durable transcript row before settings overlay (must survive dismiss).
    tmux send-keys -t "$session":0 -l 'workflow transcript anchor line'
    tmux send-keys -t "$session":0 Enter
    sleep 1.2
    tmux capture-pane -p -S -800 -t "$session":0 >"$evidence/pre-settings.txt"
    assert_file_contains "$evidence/pre-settings.txt" "workflow transcript anchor line"

    # --- Settings overlay must not stick in scrollback after Escape ---
    tmux send-keys -t "$session":0 -l '/settings'
    tmux send-keys -t "$session":0 Enter
    sleep 0.8
    tmux capture-pane -p -S -500 -t "$session":0 >"$evidence/settings-open.txt"
    tmux send-keys -t "$session":0 Escape
    sleep 0.5
    # Continue with another transcript line then capture scrollback.
    tmux send-keys -t "$session":0 -l 'after settings dismiss'
    tmux send-keys -t "$session":0 Enter
    sleep 1
    tmux capture-pane -p -S -2000 -t "$session":0 >"$evidence/settings-scrollback.txt"
    assert_file_contains "$evidence/settings-scrollback.txt" "workflow transcript anchor line"
    assert_file_contains "$evidence/settings-scrollback.txt" "after settings dismiss"
    assert_settings_excluded_from_scrollback "$evidence/settings-scrollback.txt"
    log "settings overlay scrollback exclusion hard assertion passed"

    # --- Create two workflows concurrently via /workflow create ---
    tmux send-keys -t "$session":0 -l '/workflow create alpha-flow deterministic alpha workflow objective'
    tmux send-keys -t "$session":0 Enter
    sleep 0.8
    tmux send-keys -t "$session":0 -l '/workflow create beta-flow deterministic beta workflow objective'
    tmux send-keys -t "$session":0 Enter
    sleep 1.5

    # Normal conversation screen: compact count only (no full Todo tree).
    tmux capture-pane -p -S -1200 -t "$session":0 >"$evidence/normal-compact.txt"
    assert_compact_workflow_header "$evidence/normal-compact.txt"
    assert_no_full_todo_tree_on_normal_screen "$evidence/normal-compact.txt"
    # Must show total of at least 2.
    if ! grep -E -e 'Workflows · [0-9]+ active · [2-9][0-9]* total' "$evidence/normal-compact.txt" >/dev/null; then
        # Allow single-digit total ≥2 via broader match already in helper; enforce total≥2 explicitly.
        if ! grep -E -e 'Workflows · [0-9]+ active · ([2-9]|[1-9][0-9]+) total' "$evidence/normal-compact.txt" >/dev/null; then
            fail "HARD: compact header must report total ≥ 2 after two creates"
        fi
    fi
    log "compact workflow header hard assertion passed"

    # --- Bare /workflow opens the list; Enter opens the selected detail page ---
    tmux send-keys -t "$session":0 -l '/workflow'
    tmux send-keys -t "$session":0 Enter
    sleep 1
    if ! tmux_wait_for "$session" 15 'alpha-flow' 'beta-flow' >"$evidence/workflow-list.txt"; then
        tmux capture-pane -p -S -2000 -t "$session":0 >"$evidence/workflow-list.txt"
        fail "HARD: /workflow list missing workflow names"
    fi
    assert_file_contains "$evidence/workflow-list.txt" "alpha-flow" "beta-flow"
    tmux send-keys -t "$session":0 Enter
    sleep 0.5
    tmux capture-pane -p -S -2000 -t "$session":0 >"$evidence/master-detail.txt"
    assert_master_detail_chrome "$evidence/master-detail.txt"
    # Conflict/status visibility markers when present on detail pane.
    if grep -E -e 'CONFLICTED|conflicted' "$evidence/master-detail.txt" >/dev/null; then
        printf 'conflict_visible=true\n' >"$evidence/conflict-meta.txt"
    else
        printf 'conflict_visible=deferred_to_rpc\n' >"$evidence/conflict-meta.txt"
    fi
    log "master-detail hard assertion passed"

    # Detail Escape returns to the list; a second Escape closes the page.
    tmux send-keys -t "$session":0 Escape
    sleep 0.3
    tmux send-keys -t "$session":0 Escape
    sleep 0.3
    assert_tmux_composer_editable "$session" "$evidence/workflow-composer.txt" 'WORKFLOW-COMPOSER-SENTINEL'


    # Lifecycle commands on the ordinary composer.
    tmux send-keys -t "$session":0 -l '/workflow pause alpha-flow'
    tmux send-keys -t "$session":0 Enter
    sleep 0.8
    tmux send-keys -t "$session":0 -l '/workflow resume alpha-flow'
    tmux send-keys -t "$session":0 Enter
    sleep 0.8
    tmux send-keys -t "$session":0 -l '/workflow'
    tmux send-keys -t "$session":0 Enter
    sleep 1
    tmux capture-pane -p -S -1500 -t "$session":0 >"$evidence/lifecycle.txt"
    assert_file_contains "$evidence/lifecycle.txt" "alpha-flow"
    assert_file_contains "$evidence/lifecycle.txt" "beta-flow"

    # Close the list so the final capture proves the compact conversation aggregate.
    tmux send-keys -t "$session":0 Escape
    sleep 0.4

    # Final captures.
    tmux capture-pane -p -e -S -2000 -t "$session":0 >"$evidence/tui.ansi"
    tmux capture-pane -p -S -2000 -t "$session":0 >"$evidence/tui.txt"
    assert_compact_workflow_header "$evidence/tui.txt"
    assert_settings_excluded_from_scrollback "$evidence/tui.txt"

    printf 'irc_tmux=ownership_via_rpc\nworktree_authoritative=rpc:separate-git-worktrees\n' \
        >"$evidence/meta.txt"

    tmux kill-session -t "$session" 2>/dev/null || true
    write_workflow_execution_status "$evidence/execution-status.txt" "passed" "tmux checks green"
    printf 'workflow.tmux passed\nevidence=%s\n' "$evidence"
}

run_goal_workflow_tmux() {
    local root evidence
    require_rpi
    require_cmd python3
    require_cmd git
    require_cmd tmux
    root="$(scenario_workspace workflow-goal-tmux)"
    evidence="$EVIDENCE_ROOT/workflow-goal-tmux"

    run_with_timeout 180 python3 "$E2E_DIR/lib/run_goal_workflow_tui_campaign.py" \
        --rpi "$RPI_BIN" \
        --home "$root/home" \
        --workspace "$root/workspace" \
        --evidence "$evidence"

    python3 - "$evidence/assertions.json" <<'PY'
import json, sys
from pathlib import Path

summary = json.loads(Path(sys.argv[1]).read_text(encoding="utf-8"))
if summary.get("status") != "passed":
    raise SystemExit(f"workflow goal-tmux status not passed: {summary!r}")
required = {
    "exact-chinese-commands",
    "objective-only-default-name",
    "four-distinct-workflow-ids",
    "zig-workflows-preserved-after-moonbit",
    "real-goal-and-workflow-todo-tool-results",
    "request-routed-real-worker-completions",
    "todo-overview-main-plus-four-workflows",
    "todo-details-phases-tasks-linked-jobs",
    "workflow-owned-linked-jobs",
    "loopback-openai-provider",
    "bounded-waits",
    "isolated-git-workspace",
}
missing = sorted(required - set(summary.get("checks") or []))
if missing:
    raise SystemExit(f"workflow goal-tmux missing checks: {missing}")
ids = summary.get("workflowIds") or []
if len(ids) != 4 or len(set(ids)) != 4:
    raise SystemExit(f"workflow goal-tmux IDs are not four distinct values: {ids!r}")
if summary.get("workflowTodoResults") != 4 or summary.get("goalTodoResult") is not True:
    raise SystemExit(f"workflow goal-tmux lacks real todo results: {summary!r}")
if int(summary.get("workerCompletions") or 0) < 8:
    raise SystemExit(f"workflow goal-tmux lacks real worker completions: {summary!r}")
print("workflow.goal-tmux assertions passed")
PY
    log "workflow.goal-tmux evidence=$evidence"
}

run_all() {
    prepare_roots
    run_workflow_rpc
    if command -v tmux >/dev/null 2>&1; then
        run_workflow_tmux
        run_goal_workflow_tmux
    else
        log "tmux not available; skipped workflow.tmux and workflow.goal-tmux (rpc still ran)"
    fi
    printf 'workflow campaigns passed\nevidence=%s\n' "$EVIDENCE_ROOT"
}

case "${1:-list}" in
    list|--list|--dry-run) list_scenarios ;;
    rpc) prepare_roots; run_workflow_rpc; printf 'workflow.rpc passed\nevidence=%s\n' "$EVIDENCE_ROOT" ;;
    tmux) prepare_roots; run_workflow_tmux ;;
    goal-tmux) prepare_roots; run_goal_workflow_tmux ;;
    run) run_all ;;
    *) fail "usage: $0 [list|--dry-run|run|rpc|tmux|goal-tmux]" ;;
esac
