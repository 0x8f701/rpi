#!/usr/bin/env bash
# Shared fixtures for deterministic multi-workflow campaigns.
# shellcheck shell=bash
# Sourced by E2E.d/ci scripts after common.sh.
#
# Public wire contract (product-owned; E2E asserts only):
#   status: queued|planning|running|paused|integrating|completed|failed|cancelled|conflicted
#   /workflow: bare | create <name> <objective> | list | show [id|name]
#              | pause | resume | cancel | integrate | remove
#   RPC: workflow_create|list|get|pause|resume|cancel|integrate|remove
#   ownership: workflowId + todoTaskId (camelCase WorkflowTaskOwnership)
#   compact header: Workflows · {A} active · {T} total
#   wire worktree: redacted basename label (never absolute path)
#   domain branch namespace: rpi/workflow/<workflowId>
#   creation fail-closed (no warning fallback)

# Compact normal-conversation header needles (exact prefix + active/total shape).
WORKFLOW_COMPACT_HEADER_RE='Workflows · [0-9]+ active · [0-9]+ total'

# Master-detail page chrome (WorkflowPanel contract).
WORKFLOW_PAGE_TITLE=' Workflows '
WORKFLOW_DETAIL_LABELS=(
    'Objective'
    'Status'
    'Todo'
    'Supervisor'
    'Subagents'
    'Recent IRC'
    'Worktree'
    'Integration'
)

prepare_workflow_settings() {
    local home="$1"
    mkdir -p "$home/.pi/agent"
    cat >"$home/.pi/agent/settings.json" <<'EOF'
{
  "orchestration": {
    "process": true,
    "tasks": true,
    "todo": true,
    "maxConcurrency": 4,
    "maxRecursionDepth": 2
  },
  "selector": {
    "autoSelectThreshold": 0
  },
  "workflow": {
    "enabled": true,
    "maxConcurrent": 4,
    "worktree": true,
    "failClosedWorktree": true
  }
}
EOF
}

# Trusted supervisor + worker agents for per-workflow depth-1/depth-2 trees.
# Supervisor is depth 1; task workers are depth 2 children of that supervisor.
prepare_workflow_agents() {
    local home="$1"
    local agents="$home/.pi/agent/agents"
    local skill_dir="$home/.pi/agent/skills/research"
    mkdir -p "$agents" "$skill_dir"

    cat >"$agents/supervisor.md" <<'EOF'
---
name: supervisor
description: Workflow supervisor that owns Todo DAG execution and worker directives
tools:
  - read
  - grep
  - hub
  - write
---
You are the workflow supervisor agent for deterministic multi-workflow E2E coverage.
Own the assigned workflow Todo DAG. Spawn task workers only as depth-2 children.
Send IRC directives with explicit workflowId and todoTaskId ownership. Never infer ownership from free text.
EOF

    cat >"$agents/worker.md" <<'EOF'
---
name: worker
description: Workflow task worker that executes a single owned Todo task
tools:
  - read
  - write
  - grep
  - hub
---
You are a workflow task worker for deterministic multi-workflow E2E coverage.
Execute only the Todo task identified by workflowId + todoTaskId ownership.
Acknowledge supervisor IRC directives and report completion on the same ownership pair.
EOF

    cat >"$agents/researcher.md" <<'EOF'
---
name: researcher
description: Research and study assigned topics
tools:
  - read
  - grep
  - hub
---
You are the researcher agent for deterministic workflow E2E coverage.
EOF

    cat >"$agents/writer.md" <<'EOF'
---
name: writer
description: Write assigned content
tools:
  - read
  - write
  - hub
---
You are the writer agent for deterministic workflow E2E coverage.
EOF

    cat >"$skill_dir/SKILL.md" <<'EOF'
---
name: research
description: Research topics for a researcher study
---
RESEARCH_BODY for skill-only routing checks.
EOF
}

prepare_workflow_home() {
    local home="$1"
    prepare_workflow_settings "$home"
    prepare_workflow_agents "$home"
}

# Seed a git repository in the scenario workspace so worktree creation can bind.
# Fail-closed product behavior: worktree creation failure must abort the workflow.
prepare_workflow_git_workspace() {
    local workspace="$1"
    require_cmd git
    mkdir -p "$workspace"
    if [ ! -d "$workspace/.git" ]; then
        git -C "$workspace" init -q
        git -C "$workspace" config user.email "workflow-e2e@example.com"
        git -C "$workspace" config user.name "Workflow E2E"
        printf 'workflow-e2e-workspace\n' >"$workspace/README.e2e"
        git -C "$workspace" add README.e2e
        git -C "$workspace" commit -q -m "workflow e2e seed"
    fi
}

# Deterministic overlapping ready-root Todo DAG shared by both workflows.
# Same task ids across workflows must never collide when keyed by workflowId.
workflow_todo_phases_json() {
    cat <<'EOF'
[
  {
    "name": "Roots",
    "tasks": [
      {"id": "root-a", "content": "fetch inventory", "status": "pending"},
      {"id": "root-b", "content": "compile crate", "status": "pending"}
    ]
  },
  {
    "name": "Join",
    "tasks": [
      {
        "id": "join",
        "content": "ship release",
        "status": "pending",
        "dependsOn": ["root-a", "root-b"]
      }
    ]
  }
]
EOF
}

workflow_integration_clean_marker() {
    printf 'workflow-integration-clean\n'
}

workflow_integration_conflict_marker() {
    printf 'workflow-integration-CONFLICTED\n'
}

assert_workflow_status() {
    local value="$1"
    case "$value" in
        queued|planning|running|paused|integrating|completed|failed|cancelled|conflicted) ;;
        *) fail "invalid workflow status: ${value@Q} (expected ${WORKFLOW_STATUS_ENUM})" ;;
    esac
}

assert_compact_workflow_header() {
    local path="$1"
    if ! grep -E -e "$WORKFLOW_COMPACT_HEADER_RE" "$path" >/dev/null; then
        fail "HARD: compact workflow header missing in $path (need 'Workflows · A active · T total')"
    fi
}

assert_no_full_todo_tree_on_normal_screen() {
    local path="$1"
    # Normal conversation must show only compact count/status — not full Todo/Subagents trees.
    if grep -E -e 'Todos ·' "$path" >/dev/null; then
        fail "HARD: full Todos chrome leaked onto normal conversation screen in $path"
    fi
}

assert_master_detail_chrome() {
    local path="$1"
    local label
    # The selected detail page renders one workflow and every canonical domain section.
    for label in "${WORKFLOW_DETAIL_LABELS[@]}"; do
        if ! grep -F -- "$label" "$path" >/dev/null; then
            fail "HARD: master-detail missing right-pane label ${label@Q} in $path"
        fi
    done
}

assert_settings_excluded_from_scrollback() {
    local path="$1"
    # After /settings then Escape, normal-screen scrollback MUST NOT retain
    # settings overlay chrome. Durable transcript rows may remain.
    # Contract from FixSettingsScrollback: no alternate screen; dismiss+continue
    # scrollback lacks settings needles while prior transcript stays.
    local needle
    for needle in \
        'Settings ·' \
        'Category ' \
        'Ctrl-S apply' \
        '[settings-open]' \
        'settings overlay sticky'
    do
        if grep -F -- "$needle" "$path" >/dev/null; then
            fail "HARD: settings overlay chrome leaked into scrollback (${needle@Q}) in $path"
        fi
    done
}

# Product API gate: return 0 when rpi exposes workflow RPC list successfully.
# Campaigns MUST call this before claiming executable coverage.
workflow_product_apis_available() {
    local home="$1"
    local workspace="$2"
    local probe_out probe_err
    require_rpi
    require_cmd python3
    probe_out="$(mktemp)"
    probe_err="$(mktemp)"
    # shellcheck disable=SC2064
    trap "rm -f '$probe_out' '$probe_err'" RETURN
    if run_with_timeout 20 python3 - "$RPI_BIN" "$home" "$workspace" "$probe_out" "$probe_err" <<'PY'
import json, os, subprocess, sys, time
from pathlib import Path

rpi, home, workspace, out_path, err_path = sys.argv[1:6]
env = {
    "HOME": home,
    "USERPROFILE": home,
    "PATH": os.environ.get("PATH", "/usr/bin:/bin"),
    "LANG": os.environ.get("LANG", "C.UTF-8"),
    "LC_ALL": os.environ.get("LC_ALL", "C.UTF-8"),
    "PI_CODING_AGENT_DIR": str(Path(home) / ".pi" / "agent"),
    "PI_OFFLINE": "1",
    "PI_SKIP_VERSION_CHECK": "1",
    "PI_FAUX_RESPONSE": "deterministic-workflow-probe",
    "TERM": "xterm-256color",
}
with open(err_path, "wb") as err, open(out_path, "w", encoding="utf-8") as out:
    proc = subprocess.Popen(
        [rpi, "--offline", "-C", workspace, "--model", "faux/faux-1", "--mode", "rpc"],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=err,
        text=True,
        bufsize=1,
        env=env,
    )
    assert proc.stdin and proc.stdout
    for cmd in (
        {"type": "get_commands", "id": "wf-probe-commands"},
        {"type": "workflow_list", "id": "wf-probe-list"},
    ):
        proc.stdin.write(json.dumps(cmd, separators=(",", ":")) + "\n")
        proc.stdin.flush()
        deadline = time.monotonic() + 8
        while time.monotonic() < deadline:
            line = proc.stdout.readline()
            if not line:
                break
            out.write(line)
            row = json.loads(line)
            if row.get("type") == "response" and row.get("id") == cmd["id"]:
                break
        else:
            break
    try:
        proc.stdin.close()
    except BrokenPipeError:
        pass
    try:
        proc.wait(timeout=5)
    except subprocess.TimeoutExpired:
        proc.kill()
        proc.wait(timeout=3)

text = Path(out_path).read_text(encoding="utf-8")
# Require successful workflow_list response — catalog alone is insufficient.
for line in text.splitlines():
    line = line.strip()
    if not line:
        continue
    try:
        row = json.loads(line)
    except json.JSONDecodeError:
        continue
    if (
        row.get("type") == "response"
        and row.get("id") == "wf-probe-list"
        and row.get("success") is True
    ):
        raise SystemExit(0)
raise SystemExit(1)
PY
    then
        return 0
    fi
    return 1
}

write_workflow_execution_status() {
    local path="$1"
    local status="$2"
    local detail="${3:-}"
    {
        printf 'scenario=workflow\n'
        printf 'execution_status=%s\n' "$status"
        printf 'product_apis=%s\n' "${WORKFLOW_PRODUCT_APIS:-unknown}"
        if [ -n "$detail" ]; then
            printf 'detail=%s\n' "$detail"
        fi
        printf 'note=Do not claim campaign pass until execution_status=passed after a full tmux+rpc run.\n'
    } >"$path"
}
