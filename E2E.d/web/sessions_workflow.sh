#!/usr/bin/env bash
# Web workflow DAG acceptance lane — PLAYWRIGHT-ONLY (no fallback, no skip).
#
# Extracts the workflow DAG contract (the pending "healthy Workflow DAG
# completion + fail-closed" acceptance) into a dedicated, RPC-only lane so it
# no longer depends on the full multi-session lane reaching T5.3: the flow is
# driven entirely through workflow_* RPC frames (no composer prompts, no
# streaming races).
#
# Spawns the real `rpi --listen` binary with a token file and the loopback
# mock provider (--scenario sessions; supervisor planning builds the exact
# two-task DAG compile -> ship with delegated workers held until
# POST /__release-session-workers), opens /web in a real playwright browser,
# and asserts (see sessions_workflow_test.mjs):
#   WF0 boot + token connect + primary session row + RPC-created session B
#   WF1 fail-closed probe: planning with no committed typed plan lands
#       TERMINAL failed with a retained, non-empty reason — never hangs,
#       never false-passes as completed
#   WF2 healthy multi-node DAG (compile -> ship) reaches the authoritative
#       completed state with an applied integration; held waves show live
#       progress (never completed while a task is open — no false-pass
#       status inference); task ownership is exact-once
#   WF3 second healthy workflow in session B completes and workflow_list
#       never crosses session ownership
#   WF4 mock-side counts match exactly-once (planning/workers/completed),
#       completed is terminal (status stable across re-polls)
#   WF5 workflow-panel Cancel/Remove held-response single-flight: the hold is
#       armed for the EXACT workflowId; the exact outgoing cancel/remove
#       request id and the subsequent application-socket workflow_list id are
#       recorded and both exact success responses are awaited while the
#       pre-click detail response stays physically held; the authoritative
#       list generation/status is parsed from the payload; the released
#       stale response yields exactly one fresh current-generation detail
#       matching the list (Cancel) or ZERO new detail requests/responses
#       forever (Remove)
#
# Usage: bash E2E.d/web/sessions_workflow.sh [run|list]
set -euo pipefail
SCRIPT_DIR="$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
E2E_DIR="$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd -P)"
# shellcheck source=../lib/common.sh
. "$E2E_DIR/lib/common.sh"
# shellcheck source=lib/fixture.sh
. "$SCRIPT_DIR/lib/fixture.sh"

SCENARIO="web-sessions-workflow"
TOKEN="web-sessions-workflow-e2e-token-$$-$(date +%s)"

# ---------------------------------------------------------------------------
main() {
    case "${1:-run}" in
        list|--list|--dry-run)
            printf '%s\n' \
                'web-sessions-workflow - workflow DAG acceptance: fail-closed planning probe, healthy compile->ship DAG completion with applied integration, exact-once counts + ownership, workflow_list session isolation, terminal stability (PLAYWRIGHT-ONLY, hard-fail)'
            return 0
            ;;
        run|all) ;;
        *) fail "usage: $0 [run|list]" ;;
    esac

    require_rpi
    # Playwright-only: node is REQUIRED. Missing node is a FAILURE, never a
    # skip and never a fallback to agent-browser.
    require_cmd node

    local root evidence port url pw_status=0
    root="$(scenario_workspace "$SCENARIO")"
    # Workflow storage + worktree isolation are cwd-scoped; run the fixture
    # from the seeded git workspace so workflow_create can isolate.
    git -C "$root/workspace" init -q
    git -C "$root/workspace" config user.email "web-sessions-workflow@example.com"
    git -C "$root/workspace" config user.name "Web Sessions Workflow"
    git -C "$root/workspace" config commit.gpgsign false
    printf 'web sessions workflow seed\n' >"$root/workspace/seed.txt"
    git -C "$root/workspace" add -- seed.txt
    git -C "$root/workspace" commit -q -m "web sessions workflow seed"
    evidence="$EVIDENCE_ROOT/$SCENARIO"
    printf '%s\n' "$TOKEN" >"$root/token"
    cat >"$root/home/.rpi/settings.json" <<'EOF'
{
  "orchestration": {
    "tasks": true,
    "maxConcurrency": 2,
    "maxRecursionDepth": 2,
    "isolation": "none"
  }
}
EOF

    port="$(MOCK_SCENARIO=sessions web_start_mock "$root" "$evidence")"
    web_spawn_rpi "$root" "$evidence" "$port"
    url="$(web_wait_for_listener "$evidence")"
    log "$SCENARIO: listener at $url"

    # Hard playwright run: exit 1 from web_run_playwright means the npm
    # install failed — for THIS lane that is a real failure (no fallback).
    web_run_playwright "$url" "$evidence" "$root/playwright" "$SCRIPT_DIR/sessions_workflow_test.mjs" RPI_MOCK_PORT="$port" || pw_status=$?
    if [ "$pw_status" -ne 0 ]; then
        fail "$SCENARIO: playwright lane failed (exit $pw_status) — workflow lane is playwright-only"
    fi

    web_sanity_http "$url"
    web_finish_lane "$SCENARIO"
}

main "$@"