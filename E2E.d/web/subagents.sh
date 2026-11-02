#!/usr/bin/env bash
# Focused subagents lane (playwright-only). Same steering mock and writer
# agent as before, plus a details child: task >200 chars ending DETAILSTAIL,
# long read of fixtures/subagent-details/long-relative-read-target.txt, then
# a slow hold so Stop still applies. Playwright comes from web_run_playwright
# (READY reuses a preverified tree; no npm install in this script).
#
# Usage: bash E2E.d/web/subagents.sh [run|list]
set -euo pipefail
SCRIPT_DIR="$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
E2E_DIR="$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd -P)"
# shellcheck source=../lib/common.sh
. "$E2E_DIR/lib/common.sh"
# shellcheck source=lib/fixture.sh
. "$SCRIPT_DIR/lib/fixture.sh"

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
        RPI_HOME="$root/home/.rpi" \
        RPI_OFFLINE=1 RPI_SKIP_VERSION_CHECK=1 \
        ${RPI_WEB_DEV_DIR:+RPI_WEB_DEV_DIR="$RPI_WEB_DEV_DIR"} \
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
                'web-subagents - Subagents panel: spawn/live-activity/detail-modal(accessibility+history+refresh+close)/hub-send/output-view/cancel (playwright, skip-guarded)'
            return 0
            ;;
        run|all) ;;
        *) fail "usage: $0 [run|list]" ;;
    esac

    require_rpi
    web_require_browser

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
    mkdir -p "$root/workspace/fixtures/subagent-details/workspace-relative/long-read-target-for-subagent-details-readability"
    {
      printf 'detail-read-head\n'
      printf 'detail-read-line-%02d\n' 1 2 3 4 5 6 7
      printf 'DETAILSOUTTAIL\n'
    } >"$root/workspace/fixtures/subagent-details/workspace-relative/long-read-target-for-subagent-details-readability/long-relative-read-target.txt"
    cat >"$root/home/.rpi/models.json" <<EOF
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
    cat >"$root/home/.rpi/settings.json" <<EOF
{
  "orchestration": {
    "tasks": true,
    "maxConcurrency": 2,
    "maxRecursionDepth": 2
  }
}
EOF
    mkdir -p "$root/home/.rpi/agents"
    cat >"$root/home/.rpi/agents/writer.md" <<'EOF'
---
name: writer
description: Write assigned content
tools:
  - read
  - grep
  - write
  - hub
---
You are the writer agent for deterministic web e2e coverage.
EOF
    for agent in gpt glm; do
        cat >"$root/home/.rpi/agents/$agent.md" <<EOF
---
name: $agent
description: Deterministic $agent lifecycle fixture
tools:
  - read
  - hub
---
You are the $agent agent for deterministic Web orchestration lifecycle coverage.
EOF
    done
    spawn_rpi "$root" "$evidence" "$port"
    url="$(wait_for_listener "$evidence")"
    log "web.$SCENARIO: listener at $url (token in $root/token)"

    local pw_status=0
    web_run_playwright "$url" "$evidence" "$root/playwright" "$SCRIPT_DIR/subagents_test.mjs" \
        || pw_status=$?
    if [ "$pw_status" -eq 0 ]; then
        printf 'playwright: subagents details readability: PASSED\n' | tee "$evidence/playwright-summary.txt"
    elif [ "$pw_status" -eq 1 ]; then
        fail "web.$SCENARIO: playwright SETUP FAILED (node/chromium/npm unavailable)"
    else
        fail "web.$SCENARIO: playwright lane failed (exit $pw_status)"
    fi
    local auth_status=0
    web_run_playwright "$url" "$evidence" "$root/playwright" "$SCRIPT_DIR/subagents_authority_test.mjs" \
        || auth_status=$?
    if [ "$auth_status" -ne 0 ]; then
        fail "web.$SCENARIO: authority playwright lane failed (exit $auth_status)"
    fi

    web_finish_lane "web.$SCENARIO"
}

main "$@"
