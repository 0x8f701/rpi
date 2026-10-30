#!/usr/bin/env bash
set -euo pipefail
SCRIPT_DIR="$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
. "$SCRIPT_DIR/../lib/common.sh"

list_scenarios() {
    printf '%s\n' \
        'live.architecture-research - research repo architecture and write an evidence report' \
        'live.zig-build-fix - build an isolated broken Zig fixture and repair it' \
        'live.subagent-steering - delegate a long child task, steer it, and verify the revised artifact' \
        'live.compact-overflow - soak repeated large-context turns and require a compaction event' \
        'live.typed-irc - route exact typed Hub/IRC bodies across Main and two children' \
        'live.skill-routing - discover, load, and execute a project skill, then report an unknown skill actionably' \
        'live.parallel-workflow - drive two overlapping isolated Workflows from one natural-language model turn' \
        'live.todo-child-routing - prove application-loaded public-model Todo routing in TUI and Web' \
        'live.todo-nl-mutations - prove model-driven typed Todo mutations from natural language'
}

require_live_config() {
    require_rpi
    [ -n "${RPI_LIVE_MODEL:-}" ] || fail "RPI_LIVE_MODEL is required (provider/model)"
    [ -n "${RPI_LIVE_API_KEY_ENV:-}" ] || fail "RPI_LIVE_API_KEY_ENV is required (name only, such as OPENAI_API_KEY)"
    case "$RPI_LIVE_API_KEY_ENV" in *[!A-Za-z0-9_]*|'') fail "RPI_LIVE_API_KEY_ENV must be an environment variable name" ;; esac
    [ -n "${!RPI_LIVE_API_KEY_ENV:-}" ] || fail "required model credential environment variable is unset: $RPI_LIVE_API_KEY_ENV"
}

prepare_live_settings() {
    local home="$1" reserve_tokens="${2:-16384}"
    cat > "$home/.pi/agent/settings.json" <<EOF
{"compaction":{"enabled":true,"reserveTokens":$reserve_tokens,"keepRecentTokens":4096},"orchestration":{"process":true,"tasks":true,"todo":true,"glob":true,"maxConcurrency":4,"maxRecursionDepth":2}}
EOF
}

run_prompt() {
    local name="$1" prompt="$2" root="${3:-}" approve="${4:-}" timeout_seconds="${5:-${LIVE_TIMEOUT_SECONDS:-900}}" evidence credential
    [ -n "$root" ] || root="$(scenario_workspace "$name")"
    evidence="$EVIDENCE_ROOT/$name"
    credential="${!RPI_LIVE_API_KEY_ENV}"
    prepare_live_settings "$root/home"
    printf '%s\n' "$prompt" > "$evidence/prompt.txt"
    local extra_args=()
    [ "$approve" = approve ] && extra_args+=(--approve)
    env -i HOME="$root/home" USERPROFILE="$root/home" PATH="${PATH:-/usr/bin:/bin}" LANG="${LANG:-C.UTF-8}" LC_ALL="${LC_ALL:-C.UTF-8}" PI_CODING_AGENT_DIR="$root/home/.pi/agent" RPI_SKIP_VERSION_CHECK=1 "$RPI_LIVE_API_KEY_ENV=$credential" \
        timeout --foreground --kill-after=10s "${timeout_seconds}s" "$RPI_BIN" "${extra_args[@]}" -C "$root/workspace" --model "$RPI_LIVE_MODEL" -p "$prompt" > "$evidence/output.log" 2> "$evidence/stderr.log"
}

verify_live_evidence() {
    local mode="$1" root="$2" evidence="$3"
    python3 "$SCRIPT_DIR/verify.py" "$mode" "$root/home" "$root/workspace" "$evidence"
}

todo_child_routing() {
    require_rpi
    E2E_RUN_ID="$E2E_RUN_ID" EVIDENCE_ROOT="$EVIDENCE_ROOT" WORK_ROOT="$WORK_ROOT" \
        bash "$SCRIPT_DIR/../user/todo_child_session.sh" --live
    E2E_RUN_ID="$E2E_RUN_ID" EVIDENCE_ROOT="$EVIDENCE_ROOT" WORK_ROOT="$WORK_ROOT" \
        bash "$SCRIPT_DIR/../web/todo_child_session.sh" --live
    python3 "$SCRIPT_DIR/verify.py" todo-child-routing \
        "$EVIDENCE_ROOT/todo-child-routing-tui" \
        "$EVIDENCE_ROOT/todo-child-routing-web"
}

todo_nl_mutations() {
    E2E_RUN_ID="$E2E_RUN_ID" EVIDENCE_ROOT="$EVIDENCE_ROOT" WORK_ROOT="$WORK_ROOT" \
        bash "$SCRIPT_DIR/../user/todo_nl_mutations.sh" --live
}

architecture_research() {
    run_prompt architecture-research "Inspect the repository at $REPO_ROOT using read-only tools. Produce a concise architecture report with module boundaries, entrypoints, and three cited source paths. Do not modify the repository."
}

zig_build_fix() {
    require_cmd zig
    local root evidence credential
    root="$(scenario_workspace zig-build-fix)"; evidence="$EVIDENCE_ROOT/zig-build-fix"; credential="${!RPI_LIVE_API_KEY_ENV}"; prepare_live_settings "$root/home"
    cat > "$root/workspace/main.zig" <<'EOF'
const std = @import("std");
pub fn main() !void { std.debug.print("value={d}\n", .{missing}); }
EOF
    if (cd "$root/workspace" && zig build-exe main.zig > "$evidence/before.log" 2>&1); then fail "broken Zig fixture unexpectedly compiled"; fi
    env -i HOME="$root/home" USERPROFILE="$root/home" PATH="${PATH:-/usr/bin:/bin}" PI_CODING_AGENT_DIR="$root/home/.pi/agent" RPI_SKIP_VERSION_CHECK=1 "$RPI_LIVE_API_KEY_ENV=$credential" \
        timeout --foreground --kill-after=10s "${LIVE_TIMEOUT_SECONDS:-900}s" "$RPI_BIN" -C "$root/workspace" --model "$RPI_LIVE_MODEL" -p 'Build main.zig with zig, fix the compile error at its source, rebuild it, and run the resulting program. Do not stop until it prints value=42.' > "$evidence/output.log" 2> "$evidence/stderr.log"
    (cd "$root/workspace" && zig build-exe main.zig && ./main) > "$evidence/verified.log" 2>&1
    grep -F 'value=42' "$evidence/verified.log" >/dev/null
}

subagent_steering() {
    run_prompt subagent-steering 'Use the task tool to delegate creation of steering.txt. The child should initially plan to write OLD, then use hub send or steer while it is running to change the requested content to NEW. Wait for completion, verify steering.txt contains exactly NEW, and report the child job and agent identifiers.'
}

compact_overflow() {
    local root evidence credential
    root="$(scenario_workspace compact-overflow)"; evidence="$EVIDENCE_ROOT/compact-overflow"; credential="${!RPI_LIVE_API_KEY_ENV}"; prepare_live_settings "$root/home" 120000
    python3 - "$root/workspace/big.txt" <<'PY'
from pathlib import Path
import sys
Path(sys.argv[1]).write_text(('0123456789abcdef ' * 4096 + '\n') * 16)
PY
    env -i HOME="$root/home" USERPROFILE="$root/home" PATH="${PATH:-/usr/bin:/bin}" PI_CODING_AGENT_DIR="$root/home/.pi/agent" RPI_SKIP_VERSION_CHECK=1 "$RPI_LIVE_API_KEY_ENV=$credential" \
        timeout --foreground --kill-after=10s "${LIVE_TIMEOUT_SECONDS:-1800}s" "$RPI_BIN" -C "$root/workspace" --model "$RPI_LIVE_MODEL" -p 'Repeatedly read and summarize big.txt until automatic context compaction occurs. After compaction, answer COMPACT_OK and stop.' > "$evidence/output.log" 2> "$evidence/stderr.log"
    grep -F 'COMPACT_OK' "$evidence/output.log" >/dev/null
    grep -R '"type":"compaction"' "$root/home/.pi/agent/sessions" > "$evidence/compaction-events.txt"
}

typed_irc() {
    local root evidence
    root="$(scenario_workspace typed-irc)"; evidence="$EVIDENCE_ROOT/typed-irc"
    mkdir -p "$root/home/.pi/agent/agents"
    cat > "$root/home/.pi/agent/agents/irc-alpha.md" <<'EOF'
---
name: irc-alpha
description: Typed IRC relay alpha for the live acceptance fixture.
tools: [hub]
---
You are IrcAlpha. Follow the assigned typed Hub/IRC protocol exactly. Never replace a Hub call with prose. Yield only after every required send/wait has succeeded.
EOF
    cat > "$root/home/.pi/agent/agents/irc-beta.md" <<'EOF'
---
name: irc-beta
description: Typed IRC relay beta for the live acceptance fixture.
tools: [hub]
---
You are IrcBeta. Follow the assigned typed Hub/IRC protocol exactly. Never replace a Hub call with prose. Yield only after every required send/wait has succeeded.
EOF
    run_prompt typed-irc 'Execute a real typed Hub/IRC relay; prose that merely mentions IRC is failure. Use one task batch to start exactly two children named IrcAlpha (agent irc-alpha) and IrcBeta (agent irc-beta). IrcAlpha must call hub wait from Main, receive exact body IRC_MAIN_TO_ALPHA_6F21, then call hub send to IrcBeta with exact body IRC_ALPHA_TO_BETA_6F21. The send receipt gives a typed messageId; IrcAlpha must then call hub wait from IrcBeta, receive exact body IRC_BETA_TO_ALPHA_ACK_6F21 whose replyTo matches that messageId, and finally call hub send to Main with exact body IRC_ALPHA_TO_MAIN_6F21 using replyTo equal to the beta message id before yielding. IrcBeta must call hub wait from IrcAlpha, receive exact body IRC_ALPHA_TO_BETA_6F21, call hub send to IrcAlpha with exact body IRC_BETA_TO_ALPHA_ACK_6F21 using replyTo equal to the received alpha message id, then yield. After the batch spawns, Main must call hub send to IrcAlpha with exact body IRC_MAIN_TO_ALPHA_6F21, call hub wait from IrcAlpha to receive exact body IRC_ALPHA_TO_MAIN_6F21, and wait for both jobs to settle. Finish with marker TYPED_IRC_OK_6F21 only after every typed send, wait, reply correlation, and job completion succeeded.' "$root" approve
    grep -F 'TYPED_IRC_OK_6F21' "$evidence/output.log" >/dev/null
    verify_live_evidence typed-irc "$root" "$evidence"
}

skill_routing() {
    local root evidence
    root="$(scenario_workspace skill-routing)"; evidence="$EVIDENCE_ROOT/skill-routing"
    mkdir -p "$root/workspace/.rpi/skills/route-stamp"
    cat > "$root/workspace/.rpi/skills/route-stamp/SKILL.md" <<'EOF'
---
name: route-stamp
description: Use when a request asks for a routed acceptance stamp artifact.
---
Read seed.txt. Create route-stamp.txt with exactly these three lines:
ROUTE_STAMP_BODY_6F21
seed=SKILL_SEED_6F21
status=known-skill-executed
EOF
    printf 'SKILL_SEED_6F21\n' > "$root/workspace/seed.txt"
    run_prompt skill-routing 'Create the routed acceptance stamp requested by this project. Discover the matching project skill, load it exactly once with the read tool using its skill:// URI, and execute its instructions to create the artifact. Then deliberately call the read tool exactly once for skill://missing-route-6f21 and report the resulting unknown/untrusted-skill error actionably; that negative route must not remove or alter the successful artifact. Do not guess the skill body or replace either read with prose. Finish with marker SKILL_ROUTING_OK_6F21 only after the known artifact exists and the unknown route was reported.' "$root" approve
    grep -F 'SKILL_ROUTING_OK_6F21' "$evidence/output.log" >/dev/null
    verify_live_evidence skill-routing "$root" "$evidence"
}

parallel_workflow() {
    require_cmd git
    local root evidence credential prompt timeout_seconds
    root="$(scenario_workspace parallel-workflow)"; evidence="$EVIDENCE_ROOT/parallel-workflow"; credential="${!RPI_LIVE_API_KEY_ENV}"; timeout_seconds="${LIVE_TIMEOUT_SECONDS:-1800}"
    prepare_live_settings "$root/home"
    (cd "$root/workspace" && git init -q && git config user.email live-e2e@example.invalid && git config user.name 'RPI Live E2E' && printf 'parallel workflow fixture\n' > README.txt && git add README.txt && git commit -qm 'seed live workflow fixture')
    cp "$RPI_BIN" "$root/workspace/rpi-live"
    chmod 0755 "$root/workspace/rpi-live"
    mkdir -p "$root/workspace/.rpi/skills/workflow-alpha" "$root/workspace/.rpi/skills/workflow-beta"
    cat > "$root/workspace/.rpi/skills/workflow-alpha/SKILL.md" <<'EOF'
---
name: workflow-alpha
description: Execute the alpha-6f21 isolated Workflow acceptance artifact.
---
Use the process tool to start argv ["sh","-c","sleep 12"], then use process wait for that exact supervised process. After it exits, use the write tool to create alpha-result.txt with exactly `ALPHA_WORKFLOW_FILE_6F21` plus one trailing newline. Verify the file. Finish by yielding exactly `ALPHA_WORKFLOW_ARTIFACT_6F21`. Never write either beta marker.
EOF
    cat > "$root/workspace/.rpi/skills/workflow-beta/SKILL.md" <<'EOF'
---
name: workflow-beta
description: Execute the beta-6f21 isolated Workflow acceptance artifact.
---
Use the process tool to start argv ["sh","-c","sleep 12"], then use process wait for that exact supervised process. After it exits, use the write tool to create beta-result.txt with exactly `BETA_WORKFLOW_FILE_6F21` plus one trailing newline. Verify the file. Finish by yielding exactly `BETA_WORKFLOW_ARTIFACT_6F21`. Never write either alpha marker.
EOF
    prompt="Use the actual Workflow control plane, not ordinary task delegation and not serialized prose. Use the real write tool to author nested-workflow-client.py, then the real process tool to run it. The client must start the application from the exact binary $root/workspace/rpi-live with --approve -C . --model $RPI_LIVE_MODEL --mode rpc, preserve its inherited environment and application auth plumbing, and keep stdin open. The client itself must send LF-terminated typed RPC frames and write nested-workflow-rpc.jsonl with one JSON object per outbound or inbound frame in the exact shape {\"direction\":\"send|receive\",\"frame\":{...}}. Its first two outbound Workflow control frames must both be workflow_create, before any workflow_list or workflow_detail: alpha-6f21 with objective \"Execute the workflow-alpha acceptance skill in one ready task\", then beta-6f21 with objective \"Execute the workflow-beta acceptance skill in one ready task\". It must then poll both with workflow_list and workflow_detail until terminal, save the final two detail data objects to workflow-details.json as {\"workflows\":[...]}, close nested stdin, and exit only after the nested application exits successfully. Main must use process logs and process wait for that client, inspect its files, and verify the two real Workflows completed. Keep workflow IDs, generations, branches, worktrees, Todo ownership, files, and artifacts isolated. Do not create a provider, direct task substitute, loopback planner, or fake artifact. Finish with marker PARALLEL_WORKFLOW_OK_6F21 only after the real Workflow runs completed. The nested run stores sessions, workflow records, worktrees, and workflow artifacts under the same isolated HOME but may use a different profile/root; use glob/read to inspect them rather than assuming the outer PI_CODING_AGENT_DIR path."
    printf '%s\n' "$prompt" > "$evidence/prompt.txt"
    env -i HOME="$root/home" USERPROFILE="$root/home" PATH="${PATH:-/usr/bin:/bin}" LANG="${LANG:-C.UTF-8}" LC_ALL="${LC_ALL:-C.UTF-8}" PI_CODING_AGENT_DIR="$root/home/.pi/agent" RPI_SKIP_VERSION_CHECK=1 RPI_NESTED_BIN="$root/workspace/rpi-live" RPI_NESTED_MODEL="$RPI_LIVE_MODEL" "$RPI_LIVE_API_KEY_ENV=$credential" \
        timeout --foreground --kill-after=10s "${timeout_seconds}s" "$RPI_BIN" --approve -C "$root/workspace" --model "$RPI_LIVE_MODEL" -p "$prompt" > "$evidence/output.log" 2> "$evidence/stderr.log"
    grep -F 'PARALLEL_WORKFLOW_OK_6F21' "$evidence/output.log" >/dev/null
    verify_live_evidence parallel-workflow "$root" "$evidence"
}

case "${1:-list}" in
    list|--list|--dry-run) list_scenarios ;;
    todo-child-routing) prepare_roots; todo_child_routing ;;
    todo-nl-mutations) prepare_roots; require_live_config; E2E_RUN_ID="$E2E_RUN_ID" EVIDENCE_ROOT="$EVIDENCE_ROOT" WORK_ROOT="$WORK_ROOT" bash "$SCRIPT_DIR/../user/todo_nl_mutations.sh" --live ;;
    architecture-research|zig-build-fix|subagent-steering|compact-overflow|typed-irc|skill-routing|parallel-workflow) prepare_roots; require_live_config; "${1//-/_}" ;;
    run) prepare_roots; todo_child_routing; require_live_config; architecture_research; zig_build_fix; subagent_steering; compact_overflow; typed_irc; skill_routing; parallel_workflow; todo_nl_mutations; printf 'live campaigns passed\nevidence=%s\n' "$EVIDENCE_ROOT" ;;
    *) fail "usage: $0 [list|--dry-run|run|todo-child-routing|todo-nl-mutations|architecture-research|zig-build-fix|subagent-steering|compact-overflow|typed-irc|skill-routing|parallel-workflow]" ;;
esac
