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
# Web bundle contract:
#   - The default suite builds one fixture-local production bundle from the
#     current tree, stamps it, and shares it with every child lane.
#   - An explicit RPI_WEB_DEV_DIR is never rebuilt, but must carry a matching
#     source/index fingerprint sidecar or setup fails closed.
#   - RPI_WEB_DIST_READY=1 is an internal suite marker: the parent already
#     verified the exported bundle once, so child lanes avoid redundant hashes.
#
# Playwright contract:
#   - The default suite prepares playwright + Chromium once under
#     $WORK_ROOT/playwright and reuses that node_modules tree for every lane.
#   - RPI_WEB_PLAYWRIGHT_READY=1 is an internal suite marker: the parent
#     already installed and probed Chromium, so child lanes skip npm install
#     and the launch probe.
#   - Direct lane invocation (no parent prepare) still installs and probes
#     in the lane work dir.

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
#   web_prepare_dist [output_dir]                 build once or verify supplied
#                                                   RPI_WEB_DEV_DIR; exports it
#   web_verify_dist <dir>                        fail unless bundle is tied to
#                                                   current production inputs
#   web_prepare_playwright [output_dir]           install playwright+ws once
#                                                   and probe Chromium; exports
#                                                   RPI_WEB_PLAYWRIGHT_DIR


# Hard gate: the web release lanes are playwright-only. A missing node
# runtime FAILS the lane (never skips, never falls back). The usable-Chromium
# requirement is verified by web_prepare_playwright (system Chrome/Chromium
# on PATH or playwright's bundled chromium), reported as setup failure via
# return code 1 from web_run_playwright.
web_require_browser() {
    require_cmd node
}

WEB_FIXTURE_DIR="$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
WEB_DIR="$REPO_ROOT/crates/pi-cli/web"
WEB_DIST_FINGERPRINT="$WEB_FIXTURE_DIR/web_dist_fingerprint.py"

web_verify_dist() {
    local dist="$1"
    require_cmd python3
    [ -d "$dist" ] || fail "web: supplied RPI_WEB_DEV_DIR is not a directory: $dist"
    python3 "$WEB_DIST_FINGERPRINT" verify "$WEB_DIR" "$dist" ||
        fail "web: supplied bundle is not tied to the current Web sources: $dist"
}

web_prepare_dist() {
    local output="${1:-$WORK_ROOT/web-dist}"
    if [ "${RPI_WEB_DIST_READY:-0}" = "1" ]; then
        [ -n "${RPI_WEB_DEV_DIR:-}" ] || fail "web: RPI_WEB_DIST_READY requires RPI_WEB_DEV_DIR"
        [ -s "$RPI_WEB_DEV_DIR/index.html" ] || fail "web: verified bundle is missing index.html: $RPI_WEB_DEV_DIR"
        return 0
    fi

    require_cmd python3
    if [ -n "${RPI_WEB_DEV_DIR:-}" ]; then
        local supplied="$RPI_WEB_DEV_DIR"
        python3 "$WEB_DIST_FINGERPRINT" materialize "$WEB_DIR" "$supplied" "$output" ||
            fail "web: supplied bundle is not tied to the current Web sources: $supplied"
    else
        require_cmd npm
        log "web: building one current-tree bundle for all lanes"
        (cd "$WEB_DIR" && npm ci --no-audit --no-fund && npm run build)
        [ -s "$WEB_DIR/dist/index.html" ] || fail "web: current-tree build produced no dist/index.html"
        python3 "$WEB_DIST_FINGERPRINT" stamp "$WEB_DIR" "$WEB_DIR/dist" ||
            fail "web: failed to fingerprint current-tree bundle"
        python3 "$WEB_DIST_FINGERPRINT" materialize "$WEB_DIR" "$WEB_DIR/dist" "$output" ||
            fail "web: failed to materialize current-tree bundle"
    fi
    RPI_WEB_DEV_DIR="$output"
    RPI_WEB_DIST_READY=1
    export RPI_WEB_DEV_DIR RPI_WEB_DIST_READY
}

web_playwright_chrome() {
    local chrome="" c
    for c in google-chrome google-chrome-stable chromium chromium-browser; do
        if command -v "$c" >/dev/null 2>&1; then chrome="$(command -v "$c")"; break; fi
    done
    printf '%s\n' "$chrome"
}

# Install playwright@1.55.0 + ws into $1 and probe a real Chromium launch.
# Returns 0 on success, 1 on setup failure (npm install or no usable browser).
web_install_and_probe_playwright() {
    local dest="$1" chrome="$2"
    mkdir -p "$dest"
    if ! (cd "$dest" && npm install --no-save --no-audit --no-fund --loglevel=error playwright@1.55.0 ws >/dev/null 2>&1); then
        log "${SCENARIO:-web}: playwright SETUP FAILED: npm install of playwright@1.55.0 and ws failed in $dest"
        return 1
    fi
    # Require a usable Chromium path (system Chrome/Chromium on PATH or
    # playwright's own bundled chromium, e.g. `npx playwright install
    # chromium`): probe with a real launch so a missing or non-executable
    # browser is a SETUP failure, not an assertion failure.
    if ! (cd "$dest" && RPI_CHROME="$chrome" node -e '
        const { chromium } = require("playwright");
        const opts = process.env.RPI_CHROME ? { executablePath: process.env.RPI_CHROME } : {};
        chromium.launch(opts).then((b) => b.close()).then(() => process.exit(0))
          .catch(() => process.exit(1));
    ' >/dev/null 2>&1); then
        log "${SCENARIO:-web}: playwright SETUP FAILED: no usable Chromium (system Chrome/Chromium on PATH or playwright-installed chromium required)"
        return 1
    fi
    return 0
}

# Prepare playwright once for the suite (or reuse a parent-prepared tree).
# Standalone lanes never call this; web_run_playwright falls back to a
# per-work-dir install when RPI_WEB_PLAYWRIGHT_READY is unset.
web_prepare_playwright() {
    local output="${1:-$WORK_ROOT/playwright}"
    if [ "${RPI_WEB_PLAYWRIGHT_READY:-0}" = "1" ]; then
        [ -n "${RPI_WEB_PLAYWRIGHT_DIR:-}" ] || fail "web: RPI_WEB_PLAYWRIGHT_READY requires RPI_WEB_PLAYWRIGHT_DIR"
        [ -d "$RPI_WEB_PLAYWRIGHT_DIR/node_modules/playwright" ] ||
            fail "web: verified playwright tree is missing: $RPI_WEB_PLAYWRIGHT_DIR"
        return 0
    fi
    require_cmd node
    require_cmd npm
    local chrome
    chrome="$(web_playwright_chrome)"
    log "web: preparing playwright once for all lanes in $output"
    web_install_and_probe_playwright "$output" "$chrome" ||
        fail "web: playwright SETUP FAILED (node/chromium/npm unavailable)"
    RPI_WEB_PLAYWRIGHT_DIR="$output"
    RPI_WEB_PLAYWRIGHT_READY=1
    export RPI_WEB_PLAYWRIGHT_DIR RPI_WEB_PLAYWRIGHT_READY
}



# start the loopback mock provider; writes models.json for the fixture home.
# $1 root  $2 evidence  -> prints the mock port (also written to mock-port.txt).
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
    # File-based PID (subshell-safe): web_start_mock is invoked via command
    # substitution (`port="$(web_start_mock ...)"`) so register_pid's array
    # update is lost in the parent. Persist the mock PID to a file so
    # web_kill_mock can stop it at each coverage fixture boundary (no leak).
    # Direct callers still register in the parent so cleanup_e2e reaps the mock.
    printf '%s %s\n' "$!" "$(pid_start_token "$!")" >"$root/mock.pid"
    register_pid "$!"
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
    mkdir -p "$root/home/.rpi"
    cat >"$root/home/.rpi/models.json" <<EOF
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
# -> sets RPI_PID. Parent cwd, PID list, and stdin fd stay in the calling shell.
# Knobs (lane-local, read at call time):
#   WEB_SPAWN_EXTENSION=<dir>  append --extension <dir> (xss approval fixture;
#                              other lanes that load an explicit extension)
#   WEB_SPAWN_CONTINUE=1       append --continue so the respawned listener
#                              resumes the most recent session in the cwd.
#   WEB_SPAWN_RESUME=<id>      append --resume <id> so a restart lane can
#                              select one exact listener primary. Mutually
#                              exclusive with WEB_SPAWN_CONTINUE.
#   WEB_SPAWN_TOKENLESS=1      omits --listen-token-file so the listener runs
#                              the tokenless policy (loopback accepts browsers;
#                              the page auto-connects with an empty token).
#                              Default (0) keeps the tokened fixture.

web_spawn_rpi() {
    web_prepare_dist "$WORK_ROOT/web-dist"
    local root="$1" evidence="$2" port="$3" listen_port="${4:-0}" tag="${5:-}"
    local -a extra_args=()
    if [ -n "${WEB_SPAWN_EXTENSION:-}" ]; then extra_args+=(--extension "$WEB_SPAWN_EXTENSION"); fi
    if [ "${WEB_SPAWN_CONTINUE:-0}" = "1" ] && [ -n "${WEB_SPAWN_RESUME:-}" ]; then
        fail "web: WEB_SPAWN_CONTINUE and WEB_SPAWN_RESUME are mutually exclusive"
    fi
    if [ "${WEB_SPAWN_CONTINUE:-0}" = "1" ]; then extra_args+=(--continue); fi
    if [ -n "${WEB_SPAWN_RESUME:-}" ]; then extra_args+=(--resume "$WEB_SPAWN_RESUME"); fi
    local -a token_args=()
    if [ "${WEB_SPAWN_TOKENLESS:-0}" != "1" ]; then
        token_args+=(--listen-token-file "$root/token")
    fi
    # Close the previous dynamic FIFO fd (if any) before allocating a fresh
    # fifo+fd. Kill/restart must not reuse the old descriptor or the old
    # stdin$tag path: the new listener's readiness is only observed on the
    # tagged stderr file created below.
    web_close_rpi_stdin
    local stdin_path="$root/stdin$tag"
    rm -f "$stdin_path"
    mkfifo "$stdin_path"
    # Dynamic fd holds the fifo open (read-write so the open never blocks) so
    # the REPL never sees stdin EOF and the listener stays up for the scenario.
    exec {RPI_STDIN_FD}<>"$stdin_path"
    RPI_STDIN_PATH="$stdin_path"
    # Session/worktree state is cwd-scoped; launch the fixture from its
    # isolated workspace without changing the caller's cwd. Coverage drivers
    # start several fixtures sequentially and delete each workspace after use;
    # leaking this `cd` leaves the parent shell inside a deleted directory.
    (
        cd "$root/workspace"
        exec env -i \
            HOME="$root/home" USERPROFILE="$root/home" \
            PATH="${PATH:-/usr/bin:/bin}" LANG="${LANG:-C.UTF-8}" LC_ALL="${LC_ALL:-C.UTF-8}" \
            RPI_HOME="$root/home/.rpi" \
            RPI_OFFLINE=1 RPI_SKIP_VERSION_CHECK=1 \
            RPI_WEB_DEV_DIR="${RPI_WEB_DEV_DIR:-}" \
            "$RPI_BIN" --offline "${extra_args[@]}" \
            --listen "127.0.0.1:$listen_port" \
            --listen-plaintext \
            "${token_args[@]}" \
            --model user-steering/mock --api-key user-mock-key \
            <"$stdin_path" >"$evidence/rpi$tag.stdout" 2>"$evidence/rpi$tag.stderr"
    ) &
    RPI_PID=$!
    register_pid "$RPI_PID"
}

web_close_rpi_stdin() {
    if [ -n "${RPI_STDIN_FD:-}" ]; then
        eval "exec ${RPI_STDIN_FD}>&-"
        unset RPI_STDIN_FD
    fi
    if [ -n "${RPI_STDIN_PATH:-}" ]; then
        rm -f "$RPI_STDIN_PATH"
        unset RPI_STDIN_PATH
    fi
}

web_kill_rpi() {
    web_close_rpi_stdin
    [ -n "${RPI_PID:-}" ] || return 0
    terminate_registered_pid "$RPI_PID" 0 10
    RPI_PID=""
}

# Stop the mock provider for the fixture whose workspace is $1. Reads
# "pid identity" from $root/mock.pid (web_start_mock records /proc start
# time so the lifecycle survives the command-substitution subshell) and
# signals ONLY when the live pid still carries the recorded identity — a
# stale or reused pid is never signalled; the pid file is always removed.
web_kill_mock() {
    local root="$1" pid_file="$1/mock.pid" pid token now
    [ -f "$pid_file" ] || return 0
    pid="" token=""
    read -r pid token <"$pid_file" 2>/dev/null || true
    case "$pid" in
        ''|*[!0-9]*) pid=0 ;;
    esac
    if [ "$pid" -gt 0 ] 2>/dev/null; then
        now="$(pid_start_token "$pid")"
        if [ -n "$token" ] && [ -n "$now" ] && [ "$now" = "$token" ]; then
            terminate_and_reap "$pid" 0 10
        fi
    fi
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
# When the parent suite has already prepared playwright
# (RPI_WEB_PLAYWRIGHT_READY=1), reuse that node_modules tree via a
# per-work-dir symlink and skip npm install + Chromium probe. Direct
# lane invocation still prepares prerequisites in $work.
web_run_playwright() {
    local url="$1" evidence="$2" work="$3" test_file="$4" chrome="" name
    shift 4
    # Lane env vars arrive as NAME=VAL words; `env` converts them into real
    # assignments (bash "$@" expansion would treat them as command names).
    local -a envs=("$@")
    require_cmd node
    chrome="$(web_playwright_chrome)"
    mkdir -p "$work"
    if [ "${RPI_WEB_PLAYWRIGHT_READY:-0}" = "1" ]; then
        [ -n "${RPI_WEB_PLAYWRIGHT_DIR:-}" ] || {
            log "${SCENARIO:-web}: playwright SETUP FAILED: RPI_WEB_PLAYWRIGHT_READY requires RPI_WEB_PLAYWRIGHT_DIR"
            return 1
        }
        [ -d "$RPI_WEB_PLAYWRIGHT_DIR/node_modules/playwright" ] || {
            log "${SCENARIO:-web}: playwright SETUP FAILED: verified playwright tree is missing: $RPI_WEB_PLAYWRIGHT_DIR"
            return 1
        }
        if [ "$work" != "$RPI_WEB_PLAYWRIGHT_DIR" ]; then
            rm -rf "$work/node_modules"
            ln -sfn "$RPI_WEB_PLAYWRIGHT_DIR/node_modules" "$work/node_modules"
        fi
    else
        web_install_and_probe_playwright "$work" "$chrome" || return 1
    fi
    name="$(basename "$test_file")"
    cp "$test_file" "$work/$name"
    if [ -f "$WEB_FIXTURE_DIR/session-lifecycle.mjs" ]; then
        mkdir -p "$work/lib"
        cp "$WEB_FIXTURE_DIR/session-lifecycle.mjs" "$work/lib/session-lifecycle.mjs"
    fi
    # Hard-coverage mode: preload the V8 coverage hook (node --import) and
    # point it at the per-run payload dir. Gated on RPI_COVERAGE_DIR, so
    # normal lanes are unaffected.
    local -a node_preload=()
    if [ -n "${RPI_COVERAGE_DIR:-}" ] && [ -f "$WEB_FIXTURE_DIR/coverage-hook.mjs" ]; then
        node_preload+=(--import "$WEB_FIXTURE_DIR/coverage-hook.mjs")
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

# Close the current listener stdin (REPL exits on EOF) and print the pass line.
web_finish_lane() {
    local scenario="$1"
    web_close_rpi_stdin
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
