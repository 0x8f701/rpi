#!/usr/bin/env bash
# Web client CORE E2E lane (playwright-only hard gate) — the shared
# playwright lane, invoked by the unified suite runner E2E.d/web/run.sh
# (and independently as `bash E2E.d/web/core.sh [run|list]`).
#
# Spawns the real `rpi --listen` binary with a token file and the loopback
# mock provider (E2E.d/lib/user_mock_server.py --scenario steering, which
# streams odd requests slowly over ~4s and even requests instantly), opens the
# served `/web` page in a real browser, and asserts:
#   1. the page loads (GET /web serves the self-contained client)
#   2. the WebSocket connects (Sec-WebSocket-Protocol: rpi-auth.<token>)
#   3. a prompt round-trip streams assistant text into the DOM (full reply)
#   4. abort stops a slow stream mid-flight, and a later prompt recovers
#   5. the Todo DAG panel opens and add/complete/reopen round-trip through
#      the real application (todo_op RPC) with live panel state
#   6. the rich-content renderer (markdown table, task list, mermaid, KaTeX)
#   7. the Workflow panel creates a workflow, shows it with a live status,
#      and cancel lands the cancelled status in the list (workflow_* RPC)
#   8. the Settings panel browses by category, refuses secret edits, and
#      applies a draft theme change
#   9. the Session panel renders info, renames, lists saved sessions, and
#      switches to a fresh session
#  10. the Subagents panel spawns a faux subagent, messages it, views its
#      output, and cancels it (task_spawn/hub_send/job_output/job_cancel)
#
# Browser driver: playwright via npm (ephemeral install in the scenario work
# dir) with a system Chrome/Chromium binary or playwright's bundled chromium.
# Missing node, a failed playwright install, or no usable Chromium FAILS the
# lane (exit 1, setup); assertion failures exit 2+.
#
# Usage: bash E2E.d/web/core.sh [run|list]
set -euo pipefail
SCRIPT_DIR="$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
E2E_DIR="$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd -P)"
# shellcheck source=../lib/common.sh
. "$E2E_DIR/lib/common.sh"

SCENARIO="web"
TOKEN="web-e2e-token-$$-$(date +%s)"
SLOW_TAIL="chunk-four-done"      # final chunks of the slow mock stream
FAST_REPLY="steering-followup-reply"
ABORTED_TAIL="-done"             # never rendered when abort cuts the stream

# ---------------------------------------------------------------------------
# fixture: mock provider + rpi --listen with token file
# ---------------------------------------------------------------------------
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
    # fd 9 holds the fifo open (read-write so the open never blocks) so the
    # REPL never sees stdin EOF and the listener stays up for the scenario.
    exec 9<>"$root/stdin"
    # Workflow storage + worktree isolation are cwd-scoped; run the fixture
    # from the seeded git workspace so workflow_create can isolate.
    cd "$root/workspace"
    env -i \
        HOME="$root/home" USERPROFILE="$root/home" \
        PATH="${PATH:-/usr/bin:/bin}" LANG="${LANG:-C.UTF-8}" LC_ALL="${LC_ALL:-C.UTF-8}" \
        PI_CODING_AGENT_DIR="$root/home/.pi/agent" \
        PI_OFFLINE=1 PI_SKIP_VERSION_CHECK=1 \
        RPI_WEB_DEV_DIR="${RPI_WEB_DEV_DIR:-}" \
        "$RPI_BIN" --offline \
        --listen 127.0.0.1:0 \
        --listen-token-file "$root/token" \
        --model user-steering/mock --api-key user-mock-key \
        <"$root/stdin" >"$evidence/rpi.stdout" 2>"$evidence/rpi.stderr" &
    register_pid $!
}

# Poll rpi stderr for the control-plane banner and extract the bound address.
wait_for_listener() {
    local evidence="$1" deadline=$((SECONDS + 30))
    while [ $SECONDS -lt "$deadline" ]; do
        if [ -s "$evidence/rpi.stderr" ]; then
            local banner
            banner="$(grep -m1 'Control plane listening on http://' "$evidence/rpi.stderr" || true)"
            if [ -n "$banner" ]; then
                # banner: "Control plane listening on http://127.0.0.1:PORT (...)"
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

# ---------------------------------------------------------------------------
# playwright (npm, ephemeral) — the only browser driver. Returns:
#   0 = PASSED; 1 = SETUP FAILED (node missing, no usable Chromium, or npm
#   install of playwright failed); other = the playwright test itself FAILED
#   (assertion failure). Every non-zero return fails the lane.
# ---------------------------------------------------------------------------
run_playwright() {
    local url="$1" evidence="$2" work="$3"
    require_cmd node
    local chrome=""
    for c in google-chrome google-chrome-stable chromium chromium-browser; do
        if command -v "$c" >/dev/null 2>&1; then chrome="$(command -v "$c")"; break; fi
    done
    mkdir -p "$work"
    if ! (cd "$work" && npm install --no-save --no-audit --no-fund --loglevel=error playwright@1.55.0 >/dev/null 2>&1); then
        log "web.$SCENARIO: playwright SETUP FAILED: npm install of playwright@1.55.0 failed in $work"
        return 1
    fi
    # Require a usable Chromium path (system Chrome/Chromium on PATH or
    # playwright's own bundled chromium, e.g. `npx playwright install
    # chromium`): probe with a real launch so a missing or non-executable
    # browser is a SETUP failure, not an assertion failure.
    if ! (cd "$work" && RPI_CHROME="$chrome" node -e '
        const { chromium } = require("playwright");
        const opts = process.env.RPI_CHROME ? { executablePath: process.env.RPI_CHROME } : {};
        chromium.launch(opts).then((b) => b.close()).then(() => process.exit(0))
          .catch(() => process.exit(1));
    ' >/dev/null 2>&1); then
        log "web.$SCENARIO: playwright SETUP FAILED: no usable Chromium (system Chrome/Chromium on PATH or playwright-installed chromium required)"
        return 1
    fi
    # ESM bare-specifier resolution walks up from the script's directory, so
    # run the test from the install dir where node_modules lives.
    cp "$SCRIPT_DIR/playwright_test.mjs" "$work/playwright_test.mjs"
    # Hard-coverage mode: preload the V8 coverage hook (node --import) and
    # name payloads after this lane so step 6's payload check finds them.
    local -a node_preload=()
    if [ -n "${RPI_COVERAGE_DIR:-}" ] && [ -f "$SCRIPT_DIR/lib/coverage-hook.mjs" ]; then
        node_preload+=(--import "$SCRIPT_DIR/lib/coverage-hook.mjs")
    fi
    (cd "$work" && RPI_URL="$url" RPI_TOKEN="$TOKEN" \
    RPI_SLOW_TAIL="$SLOW_TAIL" RPI_FAST_REPLY="$FAST_REPLY" RPI_ABORTED_TAIL="$ABORTED_TAIL" \
    RPI_CHROME="$chrome" RPI_EVIDENCE="$evidence" \
    RPI_COVERAGE_DIR="${RPI_COVERAGE_DIR:-}" RPI_COVERAGE_LANE="core" \
        node "${node_preload[@]}" playwright_test.mjs)
}

# ---------------------------------------------------------------------------
main() {
    case "${1:-run}" in
        list|--list|--dry-run)
            printf '%s\n' \
                'web - GET /web page load, rpi-auth subprotocol WS connect, prompt round-trip streaming, abort + recovery, todo panel, rich content, workflow create/cancel, subagents spawn/live/message/output/cancel (playwright, hard gate)'
            return 0
            ;;
        run|all) ;;
        *) fail "usage: $0 [run|list]" ;;
    esac

    require_rpi
    # The web lanes are playwright-only hard gates: node is required (fail
    # fast); the usable-Chromium requirement is verified by run_playwright.
    require_cmd node

    local root evidence port url
    root="$(scenario_workspace "$SCENARIO")"
    evidence="$EVIDENCE_ROOT/$SCENARIO"
    printf '%s\n' "$TOKEN" >"$root/token"

    port="$(start_mock "$root" "$evidence")"
    # Workflow isolation defaults to git worktrees: seed the fixture
    # workspace as a repository so workflow_create can create a worktree
    # (mirrors E2E.d/lib/run_goal_workflow_tui_campaign.py prepare_workspace).
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
    # Subagents panel e2e: enable orchestration (task_spawn/hub_send need the
    # runtime attached) and define a trusted writer agent the spawned child
    # runs as. The mock streams the child's marker prompt slowly, so the job
    # stays running long enough to assert live status and cancel it.
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

    local pw_status=0
    run_playwright "$url" "$evidence" "$root/playwright" || pw_status=$?
    if [ "$pw_status" -eq 0 ]; then
        printf 'playwright: page load, WS connect, prompt round-trip, abort, recovery, todo panel, rich content, workflow create/cancel, subagents spawn/live/message/output/cancel: PASSED\n' | tee "$evidence/playwright-summary.txt"
    elif [ "$pw_status" -eq 1 ]; then
        fail "web.$SCENARIO: playwright SETUP FAILED (node/chromium/npm unavailable)"
    else
        fail "web.$SCENARIO: playwright lane failed (exit $pw_status)"
    fi

    # Sanity: the fixture rpi is still alive and serving after the browser session.
    local status
    status="$(node -e "
        const http = require('http');
        http.get('$url', (res) => { process.stdout.write(String(res.statusCode)); process.exit(0); })
          .on('error', () => { process.stdout.write('ERR'); process.exit(0); });
    " 2>/dev/null || printf 'ERR')"
    [ "$status" = "200" ] || fail "web.$SCENARIO: fixture listener not serving after test (status=$status)"

    exec 9>&- # close stdin fifo so the REPL exits on EOF
    sleep 0.5
    printf 'web.%s passed\nevidence=%s\n' "$SCENARIO" "$evidence"
}

main "$@"
