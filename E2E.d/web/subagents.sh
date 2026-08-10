#!/usr/bin/env bash
# Focused D93 subagents-panel lane (skip-guarded) — same fixture as the core
# lane (steering mock + orchestration enabled + writer agent + real
# `rpi --listen`), but only the Subagents acceptance: spawn a faux subagent
# via the panel, assert a live job card with activity, hub-message it, view
# its output, and cancel it.
#
# Usage: bash E2E.d/web/subagents.sh [run|list]
set -euo pipefail
SCRIPT_DIR="$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
E2E_DIR="$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd -P)"
# shellcheck source=../lib/common.sh
. "$E2E_DIR/lib/common.sh"

SCENARIO="web-subagents"
TOKEN="web-subagents-token-$$-$(date +%s)"

start_mock() { # root -> prints port
    local root="$1" evidence="$2" port_file="$root/mock-port.txt" deadline port
    python3 "$E2E_DIR/lib/user_mock_server.py" --scenario steering --port-file "$port_file" \
        >"$evidence/mock-server.log" 2>&1 &
    register_pid $!
    deadline=$((SECONDS + 15))
    while [ ! -s "$port_file" ] && [ "$SECONDS" -lt "$deadline" ]; do sleep 0.2; done
    [ -s "$port_file" ] || fail "web.$SCENARIO: mock server did not write its port file"
    port="$(cat "$port_file")"
    printf '%s\n' "$port"
}

spawn_rpi() {
    local root="$1" evidence="$2" port="$3"
    mkfifo "$root/stdin"
    exec 9<>"$root/stdin"
    cd "$root/workspace"
    env -i \
        HOME="$root/home" USERPROFILE="$root/home" \
        PATH="${PATH:-/usr/bin:/bin}" LANG="${LANG:-C.UTF-8}" LC_ALL="${LC_ALL:-C.UTF-8}" \
        PI_CODING_AGENT_DIR="$root/home/.pi/agent" \
        PI_OFFLINE=1 PI_SKIP_VERSION_CHECK=1 \
        "$RPI_BIN" --offline \
        --listen 127.0.0.1:0 \
        --listen-plaintext \
        --listen-token-file "$root/token" \
        --model user-steering/mock --api-key user-mock-key \
        <"$root/stdin" >"$evidence/rpi.stdout" 2>"$evidence/rpi.stderr" &
    register_pid $!
}

wait_for_listener() {
    local evidence="$1" deadline=$((SECONDS + 30))
    while [ $SECONDS -lt "$deadline" ]; do
        if [ -s "$evidence/rpi.stderr" ]; then
            local banner
            banner="$(grep -m1 'Control plane listening on http://' "$evidence/rpi.stderr" || true)"
            if [ -n "$banner" ]; then
                local host_port
                host_port="$(printf '%s\n' "$banner" | sed -n 's/.*http:\/\/\([0-9.:]*\).*/\1/p')"
                if [ -n "$host_port" ]; then
                    printf 'http://%s/web\n' "$host_port"
                    return 0
                fi
            fi
        fi
        sleep 0.2
    done
    fail "web.$SCENARIO: rpi listener banner not seen in 30s (stderr: $(head -c 400 "$evidence/rpi.stderr" 2>/dev/null || true))"
}

main() {
    case "${1:-run}" in
        list|--list|--dry-run)
            printf '%s\n' \
                'web-subagents - Subagents panel: spawn/live-activity/hub-send/output-view/cancel (playwright/agent-browser, skip-guarded)'
            return 0
            ;;
        run|all) ;;
        *) fail "usage: $0 [run|list]" ;;
    esac

    require_rpi
    # Optional guard: require_cmd exits on failure, so probe with command -v
    # (this lane is playwright-only; without node it SKIPS instead of failing).
    if ! command -v node >/dev/null 2>&1; then
        log "web.$SCENARIO: node missing — SKIP"
        exit 0
    fi

    local root evidence port url
    root="$(scenario_workspace "$SCENARIO")"
    evidence="$EVIDENCE_ROOT/$SCENARIO"
    printf '%s\n' "$TOKEN" >"$root/token"

    port="$(start_mock "$root" "$evidence")"
    require_cmd git
    git -C "$root/workspace" init -q
    git -C "$root/workspace" config user.email "web-e2e@example.com"
    git -C "$root/workspace" config user.name "Web E2E"
    git -C "$root/workspace" config commit.gpgsign false
    printf 'web e2e seed\n' >"$root/workspace/seed.txt"
    git -C "$root/workspace" add -- seed.txt
    git -C "$root/workspace" -c commit.gpgsign=false commit -q -m seed
    cat >"$root/home/.pi/agent/models.json" <<EOF
{
  "providers": {
    "user-steering": {
      "baseUrl": "http://127.0.0.1:$port",
      "api": "openai-completions",
      "models": [
        { "id": "mock", "name": "Steering Mock", "contextWindow": 32768, "maxTokens": 2048 }
      ]
    }
  }
}
EOF
    # Orchestration must be enabled for task_spawn/hub_send, and a trusted
    # writer agent must exist for the spawned child to run as.
    cat >"$root/home/.pi/agent/settings.json" <<EOF
{
  "orchestration": {
    "tasks": true,
    "maxConcurrency": 2,
    "maxRecursionDepth": 2
  }
}
EOF
    mkdir -p "$root/home/.pi/agent/agents"
    cat >"$root/home/.pi/agent/agents/writer.md" <<'EOF'
---
name: writer
description: Write assigned content
tools:
  - read
  - write
  - hub
---
You are the writer agent for deterministic web e2e coverage.
EOF
    spawn_rpi "$root" "$evidence" "$port"
    url="$(wait_for_listener "$evidence")"
    log "web.$SCENARIO: listener at $url (token in $root/token)"

    # Prefer playwright (npm, ephemeral install in the scenario work dir);
    # require it for this focused lane (the core lane owns the agent-browser
    # fallback path).
    local chrome=""
    for c in google-chrome google-chrome-stable chromium chromium-browser; do
        if command -v "$c" >/dev/null 2>&1; then chrome="$(command -v "$c")"; break; fi
    done
    require_cmd npm
    mkdir -p "$root/playwright"
    if ! (cd "$root/playwright" && npm install --no-save --no-audit --no-fund --loglevel=error playwright@1.55.0 >/dev/null 2>&1); then
        fail "web.$SCENARIO: playwright npm install failed"
    fi
    cp "$SCRIPT_DIR/subagents_test.mjs" "$root/playwright/subagents_test.mjs"
    (cd "$root/playwright" && RPI_URL="$url" RPI_TOKEN="$TOKEN" RPI_CHROME="$chrome" RPI_EVIDENCE="$evidence" \
        node subagents_test.mjs) || fail "web.$SCENARIO: playwright lane failed"

    exec 9>&- # close stdin fifo so the REPL exits on EOF
    sleep 0.5
    printf 'web.%s passed (browser=playwright)\nevidence=%s\n' "$SCENARIO" "$evidence"
}

main "$@"
