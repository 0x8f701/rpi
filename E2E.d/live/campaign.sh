#!/usr/bin/env bash
set -euo pipefail
SCRIPT_DIR="$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
. "$SCRIPT_DIR/../lib/common.sh"
list_scenarios() {
    printf '%s\n' \
        'live.architecture-research - research repo architecture and write an evidence report' \
        'live.zig-build-fix - build an isolated broken Zig fixture and repair it' \
        'live.subagent-steering - delegate a long child task, steer it, and verify the revised artifact' \
        'live.compact-overflow - soak repeated large-context turns and require a compaction event'
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
{"compaction":{"enabled":true,"reserveTokens":$reserve_tokens,"keepRecentTokens":4096},"orchestration":{"process":true,"tasks":true,"todo":true,"maxConcurrency":4,"maxRecursionDepth":2}}
EOF
}
run_prompt() {
    local name="$1" prompt="$2" root evidence credential
    root="$(scenario_workspace "$name")"; evidence="$EVIDENCE_ROOT/$name"; credential="${!RPI_LIVE_API_KEY_ENV}"; prepare_live_settings "$root/home"
    printf '%s\n' "$prompt" > "$evidence/prompt.txt"
    env -i HOME="$root/home" USERPROFILE="$root/home" PATH="${PATH:-/usr/bin:/bin}" LANG="${LANG:-C.UTF-8}" LC_ALL="${LC_ALL:-C.UTF-8}" PI_CODING_AGENT_DIR="$root/home/.pi/agent" PI_SKIP_VERSION_CHECK=1 "$RPI_LIVE_API_KEY_ENV=$credential" \
        timeout --foreground --kill-after=10s "${LIVE_TIMEOUT_SECONDS:-900}s" "$RPI_BIN" -C "$root/workspace" --model "$RPI_LIVE_MODEL" -p "$prompt" > "$evidence/output.log" 2> "$evidence/stderr.log"
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
    env -i HOME="$root/home" USERPROFILE="$root/home" PATH="${PATH:-/usr/bin:/bin}" PI_CODING_AGENT_DIR="$root/home/.pi/agent" PI_SKIP_VERSION_CHECK=1 "$RPI_LIVE_API_KEY_ENV=$credential" \
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
    env -i HOME="$root/home" USERPROFILE="$root/home" PATH="${PATH:-/usr/bin:/bin}" PI_CODING_AGENT_DIR="$root/home/.pi/agent" PI_SKIP_VERSION_CHECK=1 "$RPI_LIVE_API_KEY_ENV=$credential" \
        timeout --foreground --kill-after=10s "${LIVE_TIMEOUT_SECONDS:-1800}s" "$RPI_BIN" -C "$root/workspace" --model "$RPI_LIVE_MODEL" -p 'Repeatedly read and summarize big.txt until automatic context compaction occurs. After compaction, answer COMPACT_OK and stop.' > "$evidence/output.log" 2> "$evidence/stderr.log"
    grep -F 'COMPACT_OK' "$evidence/output.log" >/dev/null
    grep -R '"type":"compaction"' "$root/home/.pi/agent/sessions" > "$evidence/compaction-events.txt"
}
case "${1:-list}" in
    list|--list|--dry-run) list_scenarios ;;
    architecture-research|zig-build-fix|subagent-steering|compact-overflow) prepare_roots; require_live_config; "${1//-/_}" ;;
    run) prepare_roots; require_live_config; architecture_research; zig_build_fix; subagent_steering; compact_overflow; printf 'live campaigns passed\nevidence=%s\n' "$EVIDENCE_ROOT" ;;
    *) fail "usage: $0 [list|--dry-run|run|architecture-research|zig-build-fix|subagent-steering|compact-overflow]" ;;
esac
