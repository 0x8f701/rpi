#!/usr/bin/env bash
# User-perspective, goal-driven tmux E2E lane.
#
# Each scenario is a self-contained script under E2E.d/user/ that launches the
# real rpi TUI in an isolated tmux session with either the faux provider
# (no tool calls needed) or a loopback mock provider (tool calls needed), drives
# keystrokes the way a user would, and asserts observable pane text + file side
# effects. See docs/src/user-guide/e2e-scenarios.md for the catalog.
#
# Usage: bash E2E.d/ci/user_scenarios.sh [list|run|<scenario>]
set -euo pipefail
SCRIPT_DIR="$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
E2E_DIR="$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd -P)"
# shellcheck source=../lib/common.sh
. "$E2E_DIR/lib/common.sh"

USER_SCRIPTS=(
    "$E2E_DIR/user/goal_lifecycle.sh"
    "$E2E_DIR/user/rewind_compact.sh"
    "$E2E_DIR/user/code_review_expand.sh"
    "$E2E_DIR/user/code_review_submodule.sh"
    "$E2E_DIR/user/code_review_operations.sh"
    "$E2E_DIR/user/btw_side_chat.sh"
    "$E2E_DIR/user/process_panel.sh"
    "$E2E_DIR/user/steering_queue_handoff.sh"
    "$E2E_DIR/user/bash_card_fence.sh"
    "$E2E_DIR/user/todo_dag.sh"
    "$E2E_DIR/user/todo_nl_mutations.sh"
    "$E2E_DIR/user/todo_child_session.sh"
    "$E2E_DIR/user/workflow_full_run.sh"
    "$E2E_DIR/user/project_authoring.sh"
    "$E2E_DIR/collab/collab_scenario.sh"
)

list_scenarios() {
    printf '%s\n' \
        'user.goal - goal create/pin/pause/resume/complete + budget chip + details block (faux)' \
        'user.rewind-compact - /checkpoint + /rewind picker/index/sidecar + /snapcompact + /compact A→B status (faux)' \
        'user.code-review-expand - /code-review gap above a hunk; Tab then e reveals context without dropping the hunk edit (faux)' \
        'user.code-review-submodule - /code-review review-base expands dirty ext into inner hunks, keeps clean sib-clean gitlink, Left/Right fold, End scroll, comment cancel (faux)' \
        'user.code-review-operations - /code-review Auto Review then Highlight; ■ Stop and Esc stop autoReview then highlight; hunk critical/warning/info; no Key changes card (faux)' \
        'user.btw - side chat open/submit/Esc/reopen persist + tabs new/list/switch/close (faux)' \
        'user.process - /process start then /ps overlay lists the running supervised command (faux)' \
        'user.steering-queue - follow-up queue ⚙ count, auto-drain, /queue cancel, /handoff envelope (mock)' \
        'user.bash-card - bash card comment frame + multi-line $ rows + Output separator (mock)' \
        'user.fence - unclosed code fence marker on the frame bottom (mock)' \
        'user.todo-dag - /todo overview [main] counts + detail phases/tasks + Esc navigation (mock)' \
        'user.todo-nl-mutations - natural-language prompts drive typed todo tool mutations add/remove/clear with exact persisted journal and failed-rm invariance (todo-nl mock)' \
        'user.todo-child-session - durable autonomous Todo child create/select/detail/send/add-task/IRC/lifecycle, parent switch isolation, and process restart recovery (sessions mock)' \
        'user.workflow-run - workflow create→plan→Todo DAG→integrate→completed + real git merge (mock)' \
        'user.project-authoring - empty git workspace → multi-module Rust CLI built, planted defect found via failing test, read+edit fix, tests pass, CLI valid/invalid runs, Todo DAG completed (mock)' \
        'collab - one host + two CLI guests (control+view) + one Playwright browser guest: encrypted back-history, live stream, tool card, control prompt/abort, view rejection, host-only lifecycle, disconnect/rejoin fresh epoch, participant/status, host stop, ciphertext-absence-of-plaintext, no secret/path leakage (mock+playwright)'
}

run_one() {
    local script="$1"
    [ -x "$script" ] || fail "scenario script missing or not executable: $script"
    log "user scenario: $(basename "$script")"
    "$script"
}

run_all() {
    prepare_roots
    require_cmd tmux
    local script
    for script in "${USER_SCRIPTS[@]}"; do
        run_one "$script"
    done
    printf 'user scenarios passed\nevidence=%s\n' "$EVIDENCE_ROOT"
}

case "${1:-run}" in
    list|--list|--dry-run) list_scenarios ;;
    run|all) run_all ;;
    goal) run_one "$E2E_DIR/user/goal_lifecycle.sh" ;;
    rewind-compact) run_one "$E2E_DIR/user/rewind_compact.sh" ;;
    code-review-expand) run_one "$E2E_DIR/user/code_review_expand.sh" ;;
    code-review-submodule) run_one "$E2E_DIR/user/code_review_submodule.sh" ;;
    code-review-operations) run_one "$E2E_DIR/user/code_review_operations.sh" ;;
    btw) run_one "$E2E_DIR/user/btw_side_chat.sh" ;;
    process) run_one "$E2E_DIR/user/process_panel.sh" ;;
    steering-queue) run_one "$E2E_DIR/user/steering_queue_handoff.sh" ;;
    bash-card) run_one "$E2E_DIR/user/bash_card_fence.sh" ;;
    todo-dag) run_one "$E2E_DIR/user/todo_dag.sh" ;;
    todo-nl-mutations) run_one "$E2E_DIR/user/todo_nl_mutations.sh" ;;
    todo-child-session) run_one "$E2E_DIR/user/todo_child_session.sh" ;;
    workflow-run) run_one "$E2E_DIR/user/workflow_full_run.sh" ;;
    project-authoring) run_one "$E2E_DIR/user/project_authoring.sh" ;;
    *) fail "usage: $0 [list|run|goal|rewind-compact|code-review-expand|code-review-submodule|code-review-operations|btw|process|steering-queue|bash-card|todo-dag|todo-nl-mutations|todo-child-session|workflow-run|project-authoring|collab]" ;;
esac
