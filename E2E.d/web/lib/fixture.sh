# Shared web-lane fixture helper (D97 suite aggregation).
#
# Sourced by the standalone web lanes (xss.sh, abort.sh, reconnect.sh,
# mobile.sh, auth.sh, extras.sh) and by the unified runner's lane loop.
# Every lane gets the same fixture: loopback mock provider + a real
# `rpi --listen` binary with a token file, served /web, plus the playwright
# driver (npm, ephemeral). The web lanes are playwright-only: a missing
# node runtime, failed playwright install, or no usable Chromium FAILS the
# lane (exit 1, setup); assertion failures exit 2+.
#
# Environment (set by the sourcing script before calling):
#   TOKEN   token file content (also served via rpi-auth.<token> subprotocol)
#   SCENARIO  mock provider scenario (default "steering")
#
# Functions:
#   web_start_mock <root> <evidence>            -> prints mock port; writes
#                                                  models.json for the fixture
#   web_spawn_rpi <root> <evidence> <port> [listen_port] [tag]
#                                                spawns `rpi --listen`; sets
#                                                  RPI_PID (the process id)
#   web_kill_rpi                                kills the current RPI_PID
#   web_wait_for_listener <evidence> [tag]      -> prints http://host:port/web
#   web_run_playwright <url> <evidence> <work> <test_file> [ENV=.. ..]
#                                                0 = PASSED; 1 = SETUP
#                                                FAILED (node/chromium/npm
#                                                unavailable); other = the
#                                                test FAILED (assertions)
#   web_sanity_http <url>                       GET / must return 200
#   web_lane_report <report> <scenario> <status> <evidence>   append a line
#   web_finish_lane <scenario>                  close stdin fifo + pass line
#   web_summary <report>                        print + fail on any FAIL line

# Hard gate: the web release lanes are playwright-only. A missing node
# runtime FAILS the lane (never skips, never falls back). The usable-Chromium
# requirement is verified by web_run_playwright after the ephemeral npm
# install (system Chrome/Chromium on PATH or playwright's bundled chromium),
# reported as setup failure via return code 1.
web_require_browser() {
    require_cmd node
}

# start the loopback mock provider; writes models.json for the fixture home.
# $1 root  $2 evidence  -> prints the mock port
# Knobs: MOCK_REASONING=1 marks the model reasoning (thinking levels off..high);
#        MOCK_SECOND_MODEL=1 adds a second model so model switching is real.
web_start_mock() {
    local root="$1" evidence="$2" deadline port
    # NOTE: port_file MUST be a separate statement. In `local root="$1"
    # port_file="$root/..."` the `$root` self-reference is unbound under `set -u`
    # (bash 5.2 evaluates it before the new local takes effect), so it would
    # silently fall back to a CALLING-scope `root` (dynamic scoping) — which in
    # coverage.sh is the STEERING workspace, contaminating the xss fixture's
    # mock port. Declaring root first makes port_file bind to this fixture's
    # own $1.
    local port_file="$root/mock-port.txt"
    python3 "$E2E_DIR/lib/user_mock_server.py" --scenario "${MOCK_SCENARIO:-steering}" --port-file "$port_file" \
        >"$evidence/mock-server.log" 2>&1 &
    register_pid $!
    # File-based PID (subshell-safe): web_start_mock is invoked via command
    # substitution (`port="$(web_start_mock ...)"`) so register_pid's array
    # update is lost in the parent. Persist the mock PID to a file so
    # web_kill_mock can stop it at each coverage fixture boundary (no leak).
    printf '%s\n' "$!" >"$root/mock.pid"
    deadline=$((SECONDS + 15))
    while [ ! -s "$port_file" ] && [ "$SECONDS" -lt "$deadline" ]; do sleep 0.2; done
    [ -s "$port_file" ] || fail "$SCENARIO: mock server did not write its port file"
    port="$(cat "$port_file")"
    local reasoning_attr="" second_model=""
    if [ "${MOCK_REASONING:-0}" = "1" ]; then reasoning_attr=', "reasoning": true'; fi
    if [ "${MOCK_SECOND_MODEL:-0}" = "1" ]; then
        second_model=",
        { \"id\": \"mock-2\", \"name\": \"Steering Mock Two\", \"contextWindow\": 32768, \"maxTokens\": 2048${reasoning_attr} }"
    fi
    cat >"$root/home/.pi/agent/models.json" <<EOF
{
  "providers": {
    "user-steering": {
      "baseUrl": "http://127.0.0.1:$port",
      "api": "openai-completions",
      "models": [
        { "id": "mock", "name": "Steering Mock", "contextWindow": 32768, "maxTokens": 2048${reasoning_attr} }${second_model}
      ]
    }
  }
}
EOF
    printf '%s\n' "$port"
}

# spawn `rpi --listen` against the mock. $1 root  $2 evidence  $3 mock port.
# $4 optional FIXED listen port (reconnect lane respawns on the same port;
#    default 0 = kernel-assigned). $5 optional tag ("" first spawn, "2" respawn)
# -> sets RPI_PID
# Knobs (lane-local, read at call time):
#   WEB_SPAWN_EXTENSION=<dir>  append --extension <dir> (xss lane's approval
#                              fixture extension)
#   WEB_SPAWN_CONTINUE=1       append --continue so the respawned listener
#                              resumes the most recent session in the cwd
#                              (reconnect lane: same session id -> the web
#                              client keeps the transcript across the crash)
web_spawn_rpi() {
    local root="$1" evidence="$2" port="$3" listen_port="${4:-0}" tag="${5:-}"
    local -a extra_args=()
    if [ -n "${WEB_SPAWN_EXTENSION:-}" ]; then extra_args+=(--extension "$WEB_SPAWN_EXTENSION"); fi
    if [ "${WEB_SPAWN_CONTINUE:-0}" = "1" ]; then extra_args+=(--continue); fi
    # WEB_SPAWN_TOKENLESS=1 omits --listen-token-file so the listener runs the
    # tokenless policy (loopback accepts browsers; the page auto-connects with
    # an empty token). Default (0) keeps the tokened fixture every other lane
    # relies on.
    local -a token_args=()
    if [ "${WEB_SPAWN_TOKENLESS:-0}" != "1" ]; then
        token_args+=(--listen-token-file "$root/token")
    fi
    local stdin_path="$root/stdin$tag"
    rm -f "$stdin_path"
    mkfifo "$stdin_path"
    # fd 9 holds the fifo open (read-write so the open never blocks) so the
    # REPL never sees stdin EOF and the listener stays up for the scenario.
    exec 9<>"$root/stdin$tag"
    # Session/worktree state is cwd-scoped; launch the fixture from its
    # isolated workspace without changing the caller's cwd. Coverage drivers
    # start several fixtures sequentially and delete each workspace after use;
    # leaking this `cd` leaves the parent shell inside a deleted directory.
    (
        cd "$root/workspace"
        exec env -i \
            HOME="$root/home" USERPROFILE="$root/home" \
            PATH="${PATH:-/usr/bin:/bin}" LANG="${LANG:-C.UTF-8}" LC_ALL="${LC_ALL:-C.UTF-8}" \
            PI_CODING_AGENT_DIR="$root/home/.pi/agent" \
            PI_OFFLINE=1 PI_SKIP_VERSION_CHECK=1 \
            RPI_WEB_DEV_DIR="${RPI_WEB_DEV_DIR:-}" \
            "$RPI_BIN" --offline "${extra_args[@]}" \
            --listen "127.0.0.1:$listen_port" \
            "${token_args[@]}" \
            --model user-steering/mock --api-key user-mock-key \
            <"$root/stdin$tag" >"$evidence/rpi$tag.stdout" 2>"$evidence/rpi$tag.stderr"
    ) &
    RPI_PID=$!
    register_pid $RPI_PID
}

web_kill_rpi() {
    [ -n "${RPI_PID:-}" ] || return 0
    kill "$RPI_PID" 2>/dev/null || true
    local tries=0
    while [ "$tries" -lt 10 ] && kill -0 "$RPI_PID" 2>/dev/null; do
        sleep 0.2
        tries=$((tries + 1))
    done
    kill -0 "$RPI_PID" 2>/dev/null && kill -9 "$RPI_PID" 2>/dev/null || true
    wait "$RPI_PID" 2>/dev/null || true
    RPI_PID=""
}

# Stop the mock provider for the fixture whose workspace is $1. Mirrors
# web_kill_rpi but reads the PID from $root/mock.pid (web_start_mock writes it
# there so the lifecycle survives the command-substitution subshell).
web_kill_mock() {
    local root="$1" pid_file="$1/mock.pid" pid
    [ -f "$pid_file" ] || return 0
    pid="$(cat "$pid_file" 2>/dev/null)"
    [ -n "$pid" ] || return 0
    kill "$pid" 2>/dev/null || true
    local tries=0
    while [ "$tries" -lt 10 ] && kill -0 "$pid" 2>/dev/null; do
        sleep 0.2
        tries=$((tries + 1))
    done
    kill -0 "$pid" 2>/dev/null && kill -9 "$pid" 2>/dev/null || true
    rm -f "$pid_file"
}

# Poll rpi stderr for the control-plane banner and print the /web URL.
# $1 evidence  $2 tag ("" or "2")
web_wait_for_listener() {
    local evidence="$1" tag="${2:-}" deadline=$((SECONDS + 30))
    while [ $SECONDS -lt "$deadline" ]; do
        if [ -s "$evidence/rpi$tag.stderr" ]; then
            local banner
            banner="$(grep -m1 'Control plane listening on http://' "$evidence/rpi$tag.stderr" || true)"
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
    fail "$SCENARIO: rpi listener banner not seen in 30s (stderr: $(head -c 400 "$evidence/rpi$tag.stderr" 2>/dev/null || true))"
}

# playwright (npm, ephemeral) driver — the web lanes are playwright-only.
# $1 url  $2 evidence  $3 work dir  $4 test file; extra args are ENV=VAL.
# Returns: 0 = PASSED; 1 = SETUP FAILED (node missing, no usable Chromium, or
# npm install of playwright failed); other = the playwright test itself FAILED
# (assertion failure). Every non-zero return fails the lane; 1 vs other
# distinguishes environment/setup problems from assertion failures.
web_run_playwright() {
    local url="$1" evidence="$2" work="$3" test_file="$4" chrome="" name
    shift 4
    # Lane env vars arrive as NAME=VAL words; `env` converts them into real
    # assignments (bash "$@" expansion would treat them as command names).
    local -a envs=("$@")
    require_cmd node
    for c in google-chrome google-chrome-stable chromium chromium-browser; do
        if command -v "$c" >/dev/null 2>&1; then chrome="$(command -v "$c")"; break; fi
    done
    mkdir -p "$work"
    if ! (cd "$work" && npm install --no-save --no-audit --no-fund --loglevel=error playwright@1.55.0 ws >/dev/null 2>&1); then
        log "$SCENARIO: playwright SETUP FAILED: npm install of playwright@1.55.0 and ws failed in $work"
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
        log "$SCENARIO: playwright SETUP FAILED: no usable Chromium (system Chrome/Chromium on PATH or playwright-installed chromium required)"
        return 1
    fi
    name="$(basename "$test_file")"
    cp "$test_file" "$work/$name"
    # Hard-coverage mode: preload the V8 coverage hook (node --import) and
    # point it at the per-run payload dir. Gated on RPI_COVERAGE_DIR, so
    # normal lanes are unaffected.
    local -a node_preload=()
    if [ -n "${RPI_COVERAGE_DIR:-}" ] && [ -f "$SCRIPT_DIR/lib/coverage-hook.mjs" ]; then
        node_preload+=(--import "$SCRIPT_DIR/lib/coverage-hook.mjs")
    fi
    (cd "$work" && RPI_URL="$url" RPI_TOKEN="${TOKEN:-}" \
    RPI_CHROME="$chrome" RPI_EVIDENCE="$evidence" \
    RPI_COVERAGE_DIR="${RPI_COVERAGE_DIR:-}" RPI_COVERAGE_LANE="${RPI_COVERAGE_LANE:-${name%.mjs}}" \
        env "${envs[@]}" node "${node_preload[@]}" "$name")
}

# GET / through node; fails the lane when the listener stopped serving.
web_sanity_http() {
    local url="$1" status
    status="$(node -e "
        const http = require('http');
        http.get('$url', (res) => { process.stdout.write(String(res.statusCode)); process.exit(0); })
          .on('error', () => { process.stdout.write('ERR'); process.exit(0); });
    " 2>/dev/null || printf 'ERR')"
    [ "$status" = "200" ] || fail "$SCENARIO: fixture listener not serving after test (status=$status)"
}

web_lane_report() {
    local report="$1" scenario="$2" status="$3" evidence="$4"
    printf '| %-12s | %-6s | %s |\n' "$scenario" "$status" "$evidence" >>"$report"
}

# Close the stdin fifo (REPL exits on EOF) and print the lane's pass line.
web_finish_lane() {
    local scenario="$1"
    exec 9>&-
    sleep 0.5
    printf '%s passed\nevidence=%s\n' "$scenario" "$EVIDENCE_ROOT/$scenario"
}

# Print the accumulated REPORT and exit non-zero when any lane failed.
web_summary() {
    local report="$1" failures
    printf '\n[web e2e suite report]\n'
    if [ -f "$report" ]; then
        sed 's/^/  /' "$report"
        failures="$(awk -F'|' '$3 ~ /FAIL/ { n++ } END { print n+0 }' "$report")"
    else
        failures=0
    fi
    printf '%s\n' "lanes failed: $failures (report: $report)"
    [ "$failures" -eq 0 ]
}
