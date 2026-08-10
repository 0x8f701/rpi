#!/usr/bin/env bash
# Web Goal panel E2E lane (playwright-only hard gate).
#
# Spawns the real `rpi --listen` binary with a token file and the loopback
# mock provider (E2E.d/lib/user_mock_server.py --scenario steering; no prompts
# are sent, so the mock only serves the model catalog), opens the served
# `/web` page in a real browser, and asserts the Goal panel:
#   1. empty state before any goal exists
#   2. create via the panel form (objective + token budget) renders status,
#      budget, usage, and a `created` journal entry
#   3. pin via the panel form renders the pin and a `pins_updated` entry
#   4. a goal_pause issued by a SECOND WS client flips the panel to `paused`
#      purely from the pushed goal_updated event (live updates)
#   5. resume via the panel button; journal replays created -> pins_updated
#      -> paused -> resumed in order
#
# Browser driver: playwright via npm (ephemeral install in the scenario work
# dir, incl. the `ws` package for the raw second WS client) with a system
# Chrome/Chromium binary or playwright's bundled chromium. Missing node, a
# failed playwright install, or no usable Chromium FAILS the lane (exit 1,
# setup); assertion failures exit 2+.
#
# Usage: bash E2E.d/web/goal.sh [run|list]
set -euo pipefail
SCRIPT_DIR="$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
E2E_DIR="$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd -P)"
# shellcheck source=../lib/common.sh
. "$E2E_DIR/lib/common.sh"

SCENARIO="web-goal"
TOKEN="web-goal-e2e-token-$$-$(date +%s)"

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
    [ -s "$port_file" ] || fail "web-goal: mock server did not write its port file"
    port="$(cat "$port_file")"
    printf '%s\n' "$port"
}

spawn_rpi() {
    local root="$1" evidence="$2" port="$3"
    mkfifo "$root/stdin"
    # fd 9 holds the fifo open (read-write so the open never blocks) so the
    # REPL never sees stdin EOF and the listener stays up for the scenario.
    exec 9<>"$root/stdin"
    env -i \
        HOME="$root/home" USERPROFILE="$root/home" \
        PATH="${PATH:-/usr/bin:/bin}" LANG="${LANG:-C.UTF-8}" LC_ALL="${LC_ALL:-C.UTF-8}" \
        PI_CODING_AGENT_DIR="$root/home/.pi/agent" \
        PI_OFFLINE=1 PI_SKIP_VERSION_CHECK=1 \
        RPI_WEB_DEV_DIR="${RPI_WEB_DEV_DIR:-}" \
        "$RPI_BIN" --offline \
        --listen 127.0.0.1:0 \
        --listen-plaintext \
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
    fail "web-goal: rpi listener banner not seen in 30s (stderr: $(head -c 400 "$evidence/rpi.stderr" 2>/dev/null || true))"
}

# ---------------------------------------------------------------------------
# playwright (npm, ephemeral, incl. `ws` for the second RPC client) — the
# only browser driver. Returns: 0 = PASSED; 1 = SETUP FAILED (node missing,
# no usable Chromium, or npm install of playwright failed); other = the test
# itself FAILED (assertion failure). Every non-zero return fails the lane.
# ---------------------------------------------------------------------------
run_playwright() {
    local url="$1" evidence="$2" work="$3"
    require_cmd node
    local chrome=""
    for c in google-chrome google-chrome-stable chromium chromium-browser; do
        if command -v "$c" >/dev/null 2>&1; then chrome="$(command -v "$c")"; break; fi
    done
    mkdir -p "$work"
    if ! (cd "$work" && npm install --no-save --no-audit --no-fund --loglevel=error playwright@1.55.0 ws >/dev/null 2>&1); then
        log "web-goal: playwright SETUP FAILED: npm install of playwright+ws failed in $work"
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
        log "web-goal: playwright SETUP FAILED: no usable Chromium (system Chrome/Chromium on PATH or playwright-installed chromium required)"
        return 1
    fi
    # ESM bare-specifier resolution walks up from the script's directory, so
    # run the test from the install dir where node_modules lives.
    cp "$SCRIPT_DIR/goal_test.mjs" "$work/goal_test.mjs"
    # Hard-coverage mode: preload the V8 coverage hook (node --import) and
    # name payloads after this lane so step 6's payload check finds them.
    local -a node_preload=()
    if [ -n "${RPI_COVERAGE_DIR:-}" ] && [ -f "$SCRIPT_DIR/lib/coverage-hook.mjs" ]; then
        node_preload+=(--import "$SCRIPT_DIR/lib/coverage-hook.mjs")
    fi
    (cd "$work" && RPI_URL="$url" RPI_TOKEN="$TOKEN" \
    RPI_CHROME="$chrome" RPI_EVIDENCE="$evidence" \
    RPI_COVERAGE_DIR="${RPI_COVERAGE_DIR:-}" RPI_COVERAGE_LANE="goal" \
        node "${node_preload[@]}" goal_test.mjs)
}

# ---------------------------------------------------------------------------
main() {
    case "${1:-run}" in
        list|--list|--dry-run)
            printf '%s\n' \
                'web-goal - Goal panel: empty state, create, pin, live pause event, resume, journal replay (playwright, hard gate)'
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
    spawn_rpi "$root" "$evidence" "$port"
    url="$(wait_for_listener "$evidence")"
    log "web-goal: listener at $url (token in $root/token)"

    local pw_status=0
    local objective="ship the web goal e2e $$" pin="stay focused"
    run_playwright "$url" "$evidence" "$root/playwright" || pw_status=$?
    if [ "$pw_status" -eq 0 ]; then
        printf 'playwright: goal panel empty state, create, pin, live pause, resume, journal replay: PASSED\n' | tee "$evidence/playwright-summary.txt"
    elif [ "$pw_status" -eq 1 ]; then
        fail "web-goal: playwright SETUP FAILED (node/chromium/npm unavailable)"
    else
        fail "web-goal: playwright lane failed (exit $pw_status)"
    fi

    # Sanity: the fixture rpi is still alive and serving after the browser session.
    local status
    status="$(node -e "
        const http = require('http');
        http.get('$url', (res) => { process.stdout.write(String(res.statusCode)); process.exit(0); })
          .on('error', () => { process.stdout.write('ERR'); process.exit(0); });
    " 2>/dev/null || printf 'ERR')"
    [ "$status" = "200" ] || fail "web-goal: fixture listener not serving after test (status=$status)"

    exec 9>&- # close stdin fifo so the REPL exits on EOF
    sleep 0.5
    printf 'web-goal passed\nevidence=%s\n' "$evidence"
}

main "$@"
