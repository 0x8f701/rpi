#!/usr/bin/env bash
# Hard Web coverage command — measures REAL line/function/branch coverage of
# the web client (crates/pi-cli/web/src/**/*.{ts,tsx}) through the REAL
# `rpi --listen` binary + loopback mock + REAL Playwright assertions, and
# enforces explicit Istanbul thresholds.
#
# Pipeline (every step is a hard gate — no skips, no agent-browser fallback):
#   1. build a TEMPORARY conditionally-instrumented bundle
#      (vite.coverage.config.ts: inline source map, no minification) into
#      $WORK_ROOT — the normal dist/ is never touched
#   2. verify playwright + chromium launch (hard fail)
#   3. run the matrix driver (coverage_test.mjs) against the steering fixture
#      (own mock + rpi --listen serving the coverage bundle via
#      RPI_WEB_DEV_DIR), including a REAL server kill/respawn reconnect
#   4. run the XSS matrix driver (coverage_xss.mjs) against the xss scenario
#      + fixture approval extension
#   5. run the REAL web lane suite (E2E.d/web/run.sh, every lane) against the
#      same coverage bundle — each lane must PASS and must produce a coverage
#      payload (a lane that skipped or fell back fails the coverage run); the
#      sessions lane additionally writes its executed-assertion evidence
#      ($EVIDENCE_ROOT/web-sessions/coverage-assertions.json)
#   6. merge every V8 payload, convert through the inline source map
#      (scripts/coverage-report.mjs), verify source mapping for every expected
#      src file, emit text + JSON summary + lcov, enforce thresholds — the
#      GLOBAL thresholds (coverage.config.mjs thresholds) AND per-file hard
#      thresholds (coverage.config.mjs fileThresholds): scrollPin.ts is gated
#      at >=90% lines/functions/branches/statements. Any scroll metric below
#      its gate fails this command (exit 2). The REAL scroll browser lane
#      payload (scroll_test.mjs, run in step 5) is asserted present and merged
#      here.
#   7. validate the feature matrix against the executed assertion evidence
#      (coverage_matrix.mjs) — zero uncovered required assertions across the
#      steering/xss/fallback drivers AND the sessions + scroll lanes
#
# The normal bundle is rebuilt afterwards by the packaging owner
# (FixWebDistPackaging); this command never writes dist/.
#
# Usage: bash E2E.d/web/coverage.sh [run|list]
set -euo pipefail
SCRIPT_DIR="$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
E2E_DIR="$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd -P)"
# shellcheck source=../lib/common.sh
. "$E2E_DIR/lib/common.sh"
# shellcheck source=lib/fixture.sh
. "$SCRIPT_DIR/lib/fixture.sh"

WEB_DIR="$REPO_ROOT/crates/pi-cli/web"
# NOTE: COV_DIST and PAYLOADS_DIR MUST live OUTSIDE $WORK_ROOT. Step 5 runs the
# real lanes as child processes (run.sh -> bash <lane>.sh) that each inherit
# the cleanup_e2e EXIT trap and `rm -rf $WORK_ROOT` on exit (and re-source
# common.sh, which resets E2E_CLEANUP_PATHS to just $WORK_ROOT). If the coverage
# bundle / payloads were under $WORK_ROOT, the first lane's exit would delete
# them for every later lane (and the merge). Keep them under the persistent
# evidence root; register them so coverage.sh's own final trap still cleans up.
EVIDENCE_DIR="$EVIDENCE_ROOT/coverage"
PAYLOADS_DIR="$EVIDENCE_DIR/coverage-payloads"
COV_DIST="$EVIDENCE_DIR/web-coverage-dist"
register_cleanup_path "$PAYLOADS_DIR"
register_cleanup_path "$COV_DIST"

# Steering fixture knobs: the matrix driver's model/thinking switch phase
# needs the second (reasoning) model.
export MOCK_SCENARIO=steering MOCK_SECOND_MODEL=1 MOCK_REASONING=1
export RPI_COVERAGE_DIR="$PAYLOADS_DIR"

TOKEN="web-coverage-token-$$-$(date +%s)"
WRONG_TOKEN="web-coverage-wrong-token"

list_mode() {
    printf '%s\n' \
        'web-coverage - hard source coverage of src/**/*.{ts,tsx} via real rpi --listen + Playwright lanes (Istanbul line/function/branch, enforced thresholds, feature matrix)'
}

require_node_npm() {
    require_cmd node
    require_cmd npm
    require_cmd python3
}

# Build the temporary coverage bundle (inline source map, unminified).
build_coverage_bundle() {
    rm -rf "$COV_DIST"
    mkdir -p "$COV_DIST"
    log "coverage: building coverage bundle into $COV_DIST"
    (cd "$WEB_DIR" && RPI_COVERAGE_OUT="$COV_DIST" npx vite build --config vite.coverage.config.ts)
    [ -s "$COV_DIST/index.html" ] || fail "coverage: built bundle has no index.html"
    if ! node -e "
        const fs = require('fs');
        const html = fs.readFileSync('$COV_DIST/index.html', 'utf8');
        if (!/sourceMappingURL=data:application\/json;charset=utf-8;base64,/.test(html)) process.exit(1);
    "; then
        fail "coverage: built bundle lost its inline source map (source mapping unavailable)"
    fi
    log "coverage: bundle built + inline source map verified"
}

# Verify playwright installs and chromium actually launches (hard fail).
verify_playwright() {
    local work="$1" chrome="$2"
    mkdir -p "$work"
    if ! (cd "$work" && npm install --no-save --no-audit --no-fund --loglevel=error playwright@1.55.0 >/dev/null 2>&1); then
        fail "coverage: playwright install failed — coverage requires playwright (no fallback)"
    fi
    cat >"$work/chromium-check.mjs" <<EOF
import { chromium } from 'playwright';
const opts = process.env.RPI_CHROME ? { executablePath: process.env.RPI_CHROME } : {};
const browser = await chromium.launch(opts);
await browser.close();
console.log('chromium launch ok');
EOF
    if ! (cd "$work" && RPI_CHROME="$chrome" node chromium-check.mjs >/dev/null 2>&1); then
        fail "coverage: chromium unavailable (no system chrome and playwright's chromium did not launch)"
    fi
    log "coverage: playwright + chromium verified"
}

# Run one driver via the fixture's playwright runner and fail hard on any
# non-zero result (1 = playwright unavailable, 2 = assertion failure).
run_driver() { # lane url evidence work testfile [env...]
    local lane="$1" url="$2" evidence="$3" work="$4" testfile="$5"
    shift 5
    local pw_status=0
    RPI_COVERAGE_LANE="$lane" RPI_WORK="$work" \
        web_run_playwright "$url" "$evidence" "$work" "$testfile" "$@" || pw_status=$?
    if [ "$pw_status" -eq 0 ]; then
        log "coverage: driver '$lane' PASSED"
    elif [ "$pw_status" -eq 1 ]; then
        fail "coverage: driver '$lane' — playwright SETUP FAILED (node/chromium/npm unavailable)"
    else
        fail "coverage: driver '$lane' failed (exit $pw_status)"
    fi
}

# Real lane suite with the coverage bundle; every lane must pass AND produce
# a payload.
run_real_lanes() {
    log "coverage: running the real web lane suite against the coverage bundle"
    # The lanes (run.sh -> bash <lane>.sh) re-source common.sh and would
    # otherwise re-roll E2E_RUN_ID/EVIDENCE_ROOT. Keep evidence under THIS
    # run so the sessions assertion file reaches step 7, but give child lanes
    # a disposable work subtree. Each lane's EXIT trap deletes its WORK_ROOT;
    # sharing the parent's WORK_ROOT would delete the coverage fixture cwd and
    # break later lanes with getcwd errors.
    local lanes_work_root="$WORK_ROOT/real-lanes"
    if ! E2E_RUN_ID="$E2E_RUN_ID" EVIDENCE_ROOT="$EVIDENCE_ROOT" WORK_ROOT="$lanes_work_root" bash "$SCRIPT_DIR/run.sh" run; then
        fail "coverage: one or more real web lanes FAILED (see report)"
    fi
}

# The reconnect phase of the matrix driver: watch kill-server.marker, kill
# and respawn the listener on the SAME port with --continue, then write
# server-up.marker. Mirrors E2E.d/web/reconnect.sh's proven handshake.
watch_reconnect() { # pw_pid url root evidence port marker_dir
    local pw_pid="$1" url="$2" root="$3" evidence="$4" port="$5" marker_dir="$6"
    local deadline=$((SECONDS + 300))
    while [ ! -f "$marker_dir/kill-server.marker" ] && [ "$SECONDS" -lt "$deadline" ]; do
        if ! kill -0 "$pw_pid" 2>/dev/null; then break; fi
        sleep 0.2
    done
    if [ -f "$marker_dir/kill-server.marker" ]; then
        local listen_port
        listen_port="$(printf '%s' "$url" | sed -n 's#http://[^:]*:\([0-9]*\)/web#\1#p')"
        log "coverage: driver requested a server kill — killing rpi (port $listen_port), respawning"
        web_kill_rpi
        sleep 1
        WEB_SPAWN_CONTINUE=1 web_spawn_rpi "$root" "$evidence" "$port" "$listen_port" "2"
        web_wait_for_listener "$evidence" "2" >/dev/null
        touch "$marker_dir/server-up.marker"
    fi
}

# ---------------------------------------------------------------------------
main() {
    case "${1:-run}" in
        list|--list|--dry-run) list_mode; return 0 ;;
        run|all) ;;
        *) fail "usage: $0 [run|list]" ;;
    esac

    require_rpi
    require_node_npm
    prepare_roots
    mkdir -p "$EVIDENCE_DIR" "$PAYLOADS_DIR"

    local chrome="" c
    for c in google-chrome google-chrome-stable chromium chromium-browser; do
        if command -v "$c" >/dev/null 2>&1; then chrome="$(command -v "$c")"; break; fi
    done

    build_coverage_bundle
    verify_playwright "$WORK_ROOT/playwright-precheck" "$chrome"
    export RPI_WEB_DEV_DIR="$COV_DIST"

    # ---- 3. steering matrix driver (own fixture, real reconnect) ----
    local root evidence port url
    root="$(scenario_workspace "coverage")"
    evidence="$EVIDENCE_DIR/driver-steering"
    mkdir -p "$evidence"
    require_cmd git
    git -C "$root/workspace" init -q
    git -C "$root/workspace" config user.email "web-coverage@example.com"
    git -C "$root/workspace" config user.name "Web Coverage"
    git -C "$root/workspace" config commit.gpgsign false
    printf 'web coverage seed\n' >"$root/workspace/seed.txt"
    git -C "$root/workspace" add -- seed.txt
    git -C "$root/workspace" -c commit.gpgsign=false commit -q -m seed
    cat >"$root/home/.pi/agent/settings.json" <<EOF
{
  "orchestration": {
    "tasks": true,
    "maxConcurrency": 2,
    "maxRecursionDepth": 2
  },
  "compaction": {
    "snapKeepTurns": 1
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
You are the writer agent for deterministic web coverage.
EOF
    printf '%s\n' "$TOKEN" >"$root/token"
    # Realtime coverage: the mock's /v1/realtime/calls branch records the
    # proxy's request headers here so coverage_test.mjs can assert
    # OpenAI-Alpha: quicksilver=v2 + the Bearer token reached the backend.
    export MOCK_REALTIME_EVIDENCE="$evidence/realtime-call.json"
    port="$(web_start_mock "$root" "$evidence")"
    # Advertise Codex Live realtime mode to the web client at boot: the
    # steering driver's realtime scenario clicks the composer mic and drives
    # a REAL realtime_create_call RPC (Rust proxy -> this mock's create-call
    # endpoint). allowInsecure is required for the loopback http:// base URL.
    python3 - "$root/home/.pi/agent/settings.json" "$port" <<'PY'
import json, sys
path, port = sys.argv[1], sys.argv[2]
settings = json.load(open(path, encoding="utf-8"))
settings["live"] = {
    "enabled": True,
    "mode": "realtime",
    "realtimeBaseUrl": f"http://127.0.0.1:{port}",
    "realtimeApiKey": "mock-realtime-key",
    "realtimeModel": "gpt-realtime-1.5",
    "voice": "sol",
    "allowInsecure": True,
}
json.dump(settings, open(path, "w", encoding="utf-8"))
PY
    web_spawn_rpi "$root" "$evidence" "$port"
    url="$(web_wait_for_listener "$evidence")"
    log "coverage: steering fixture listener at $url"

    local pw_pid=0 pw_status=0
    RPI_COVERAGE_LANE="coverage-main" RPI_WORK="$root/playwright" \
        web_run_playwright "$url" "$evidence" "$root/playwright" "$SCRIPT_DIR/coverage_test.mjs" \
        RPI_WRONG_TOKEN="$WRONG_TOKEN" >"$evidence/driver.out" 2>&1 &
    pw_pid=$!
    watch_reconnect "$pw_pid" "$url" "$root" "$evidence" "$port" "$root/playwright"
    wait "$pw_pid" || pw_status=$?
    if [ "$pw_status" -eq 0 ]; then
        log "coverage: steering matrix driver PASSED"
    elif [ "$pw_status" -eq 1 ]; then
        fail "coverage: steering matrix driver — playwright SETUP FAILED"
    else
        cat "$evidence/driver.out" >&2 || true
        fail "coverage: steering matrix driver failed (exit $pw_status)"
    fi
    web_sanity_http "$url"
    web_kill_rpi
    web_kill_mock "$root"

    # ---- 4. xss matrix driver (xss scenario + approval extension) ----
    local xroot xevidence xport xurl
    xroot="$(scenario_workspace "coverage-xss")"
    xevidence="$EVIDENCE_DIR/driver-xss"
    mkdir -p "$xevidence"
    printf '%s\n' "$TOKEN" >"$xroot/token"
    mkdir -p "$xroot/ext"
    cat >"$xroot/ext/pi-extension.json" <<EOF
{"schemaVersion":1,"id":"web-xss-approval","runtime":"quickjs","entry":"index.mjs","capabilities":["ui","event_hooks"],"uiCapabilities":["confirm"]}
EOF
    cat >"$xroot/ext/index.mjs" <<EOF
export default function (pi) {
  pi.on("input", async (event, ctx) => {
    const text = event && typeof event.text === "string" ? event.text : "";
    if (!text.includes("REQUEST_APPROVAL")) return;
    try {
      await ctx.ui.confirm(
        "Approve <img src=x onerror=window.__xss2='pwned'>?",
        "proceed <script>window.__xss2='pwned'</script> with " + "s" + "k" + "-" + "approval-secret-abcdef0123456789",
        { timeout: 2000 }
      );
    } catch (e) {
      // The web client renders "answer in the terminal" and never answers;
      // the 2s timeout lets the turn settle so the prompt round-trips.
    }
  });
}
EOF
    # MOCK_SCENARIO=xss MUST be inside the command substitution (as a prefix
    # to the web_start_mock command). As a prefix to an assignment-only
 # command (`MOCK_SCENARIO=xss xport=...`) bash would persist it in this
 # shell (and it is exported), leaking scenario=xss into every step-5 lane
 # (their mocks would return the instant XSS payload instead of a slow
 # stream -> "reply never streamed"). As a command prefix it is temporary.
    xport="$(MOCK_SCENARIO=xss web_start_mock "$xroot" "$xevidence")"
    WEB_SPAWN_EXTENSION="$xroot/ext" web_spawn_rpi "$xroot" "$xevidence" "$xport"
    xurl="$(web_wait_for_listener "$xevidence")"
    log "coverage: xss fixture listener at $xurl"
    pw_status=0
    RPI_COVERAGE_LANE="coverage-xss" \
        web_run_playwright "$xurl" "$xevidence" "$xroot/playwright" "$SCRIPT_DIR/coverage_xss.mjs" \
        RPI_XSS_TEXT="unsafe <img src=x onerror=alert(1)><script>window.__xss='pwned'</script> and the leaked credential $(printf '%s%s%s' s k -)test-secret-""abcdef0123456789." \
        RPI_XSS_SECRET="$(printf '%s%s%s' s k -)test-secret-""abcdef0123456789" \
        RPI_APPROVAL_MARKER="REQUEST_APPROVAL" RPI_APPROVAL_EXT_ID="web-xss-approval" \
        RPI_APPROVAL_SECRET="$(printf '%s%s%s' s k -)approval-""secret-abcdef0123456789" \
        >"$xevidence/driver.out" 2>&1 || pw_status=$?
    if [ "$pw_status" -eq 0 ]; then
        log "coverage: xss matrix driver PASSED"
    elif [ "$pw_status" -eq 1 ]; then
        fail "coverage: xss matrix driver — playwright SETUP FAILED"
    else
        cat "$xevidence/driver.out" >&2 || true
        fail "coverage: xss matrix driver failed (exit $pw_status)"
    fi
    web_sanity_http "$xurl"
    web_kill_rpi
    web_kill_mock "$xroot"

    # ---- 4c. fallback coverage driver (zero-hit panel margin) ----
    # An INDEPENDENT steering-fixture driver (coverage_fallback.mjs) that
    # closes the zero-hit margin on panels the core steering driver does not
    # exhaustively exercise: SessionPanel refresh/rename-Enter/clone/fork/
    # switch-row, GoalPanel unpin, SideChatPanel Enter-prompt/tab-switch/
    # tab-close, redact credential-shape branches through real panel safeText
    # rendering, and the App panel close callbacks. Uses its OWN dedicated
    # deterministic steering fixture (own mock + rpi --listen on a fresh port)
    # so it never collides with the core driver's reconnect sequence; collects
    # V8 coverage through the same standard hook (lane = coverage-fallback).
    # No overlap with the core driver's files — coverage_test.mjs /
    # coverage_xss.mjs are untouched.
    local froot fevidence fport furl pw_status=0
    froot="$(scenario_workspace "coverage-fallback")"
    fevidence="$EVIDENCE_DIR/driver-fallback"
    mkdir -p "$fevidence"
    printf '%s\n' "$TOKEN" >"$froot/token"
    require_cmd git
    git -C "$froot/workspace" init -q
    git -C "$froot/workspace" config user.email "web-coverage@example.com"
    git -C "$froot/workspace" config user.name "Web Coverage"
    git -C "$froot/workspace" config commit.gpgsign false
    printf 'web coverage fallback seed\n' >"$froot/workspace/seed.txt"
    git -C "$froot/workspace" add -- seed.txt
    git -C "$froot/workspace" -c commit.gpgsign=false commit -q -m seed
    fport="$(web_start_mock "$froot" "$fevidence")"
    web_spawn_rpi "$froot" "$fevidence" "$fport"
    furl="$(web_wait_for_listener "$fevidence")"
    log "coverage: fallback fixture listener at $furl"
    RPI_COVERAGE_LANE="coverage-fallback" RPI_WORK="$froot/playwright" \
        web_run_playwright "$furl" "$fevidence" "$froot/playwright" "$SCRIPT_DIR/coverage_fallback.mjs" \
        >"$fevidence/driver.out" 2>&1 || pw_status=$?
    if [ "$pw_status" -eq 0 ]; then
        log "coverage: fallback matrix driver PASSED"
    elif [ "$pw_status" -eq 1 ]; then
        fail "coverage: fallback matrix driver — playwright SETUP FAILED"
    else
        cat "$fevidence/driver.out" >&2 || true
        fail "coverage: fallback matrix driver failed (exit $pw_status)"
    fi
    web_sanity_http "$furl"
    web_kill_rpi
    web_kill_mock "$froot"

    # ---- 4b. collab browser guest coverage driver ----
    # The real encrypted browser guest scenario (one host + two CLI guests +
    # one Playwright control guest) already drives the CollabGuestView/collab
    # happy path end-to-end. Run it under the coverage env so the browser guest
    # loads the instrumented bundle (host serves RPI_WEB_DEV_DIR/index.html at
    # the /collab/ws/<roomId> document path) and the coverage hook collects V8
    # coverage into the shared payload dir (lane = coverage-collab). The child
    # gets its OWN WORK_ROOT/EVIDENCE_ROOT (E2E_RUN_ID/WORK_ROOT/EVIDENCE_ROOT
    # are intentionally NOT exported, so its cleanup trap cannot delete this
    # command's coverage fixtures or payloads — same isolation the step-5
    # lanes rely on); only RPI_BIN/RPI_WEB_DEV_DIR/RPI_COVERAGE_DIR are passed.
    local collab_status=0
    RPI_BIN="$RPI_BIN" RPI_WEB_DEV_DIR="$COV_DIST" \
        RPI_COVERAGE_DIR="$PAYLOADS_DIR" RPI_COVERAGE_LANE="coverage-collab" \
        COLLAB_VIEW_GUEST=1 \
        bash "$E2E_DIR/collab/collab_scenario.sh" run \
        >"$EVIDENCE_DIR/driver-collab.out" 2>&1 || collab_status=$?
    if [ "$collab_status" -eq 0 ]; then
        log "coverage: collab browser guest driver PASSED"
    else
        cat "$EVIDENCE_DIR/driver-collab.out" >&2 || true
        fail "coverage: collab browser guest driver failed (exit $collab_status)"
    fi

    # ---- 5. real lane suite ----
    run_real_lanes

    # ---- payload verification: every lane must have produced coverage ----
    local lanes
    lanes="$(sed -n 's/^LANES="\(.*\)"/\1/p' "$SCRIPT_DIR/run.sh")"
    [ -n "$lanes" ] || fail "coverage: could not extract the Web lane registry from run.sh"
    local lane testfile missing=0
    for lane in $lanes; do
        testfile="$(sed -n "/web_run_playwright/,/\.mjs\"/ { s#.*\"\$SCRIPT_DIR/\([a-z0-9_]*\.mjs\)\".*#\1#p; }" "$SCRIPT_DIR/$lane.sh" | head -1)"
        if [ -z "$testfile" ]; then
            if [ -f "$SCRIPT_DIR/${lane}_test.mjs" ]; then testfile="${lane}_test.mjs"; else testfile="${lane}.mjs"; fi
        fi
        if ! ls "$PAYLOADS_DIR/${testfile%.mjs}-"*.json >/dev/null 2>&1 \
            && ! ls "$PAYLOADS_DIR/$lane-"*.json >/dev/null 2>&1; then
            log "coverage: FAIL: lane '$lane' produced no coverage payload (expected ${testfile%.mjs}-*.json or $lane-*.json)"
            missing=1
        fi
    done
    for driver_lane in coverage-main coverage-xss coverage-fallback coverage-collab; do
        if ! ls "$PAYLOADS_DIR/$driver_lane-"*.json >/dev/null 2>&1; then
            log "coverage: FAIL: driver '$driver_lane' produced no coverage payload"
            missing=1
        fi
    done
    [ "$missing" -eq 0 ] || fail "coverage: one or more lanes produced no coverage payload (skipped/fallback lanes are not counted)"

    # The per-file hard gate (scrollPin.ts >= 90% lines/functions/branches/
    # statements) depends on the REAL scroll browser lane's V8 payload being
    # present and merged. Assert it explicitly (the generic loop above already
    # checks it, but a named assertion makes a missing scroll payload an
    # unambiguous failure rather than a generic "lane produced no coverage").
    if ! ls "$PAYLOADS_DIR"/scroll_test-*.json >/dev/null 2>&1 \
        && ! ls "$PAYLOADS_DIR"/scroll-*.json >/dev/null 2>&1; then
        fail "coverage: scroll lane produced no V8 payload — scrollPin.ts per-file gate cannot be evaluated"
    fi
    log "coverage: scroll lane V8 payload present (scrollPin.ts per-file gate will be evaluated)"

    # ---- 6. merge + report + thresholds ----
    local report_dir="$EVIDENCE_DIR/report"
    node "$WEB_DIR/scripts/coverage-report.mjs" \
        --payloads "$PAYLOADS_DIR" \
        --dist "$COV_DIST/index.html" \
        --web-root "$WEB_DIR" \
        --config "$WEB_DIR/coverage.config.mjs" \
        --out "$report_dir"

    # ---- 7. matrix validation ----
    # The sessions lane writes its own executed-assertion evidence
    # ($EVIDENCE_ROOT/web-sessions/coverage-assertions.json) during the
    # step-5 real lane suite; it is gated here like the two drivers.
    node "$SCRIPT_DIR/coverage_matrix.mjs" \
        --evidence "$EVIDENCE_DIR/driver-steering/coverage-assertions.json" \
        --evidence "$EVIDENCE_DIR/driver-xss/coverage-assertions.json" \
        --evidence "$EVIDENCE_DIR/driver-fallback/coverage-assertions.json" \
        --evidence "$EVIDENCE_ROOT/web-sessions/coverage-assertions.json" \
        --evidence "$EVIDENCE_ROOT/web-scroll/coverage-assertions.json" \
        --evidence "$EVIDENCE_ROOT/web-projects/coverage-assertions.json" \
        --evidence "$EVIDENCE_ROOT/web-external_sessions/coverage-assertions.json" \
        --evidence "$EVIDENCE_ROOT/web-presentation/coverage-assertions.json" \
        --evidence "$EVIDENCE_ROOT/web-loop_goal/coverage-assertions.json"

    # persist payloads next to the report for later inspection
    mkdir -p "$EVIDENCE_DIR/payloads"
    cp "$PAYLOADS_DIR/"*.json "$EVIDENCE_DIR/payloads/" 2>/dev/null || true

    log "coverage: COMPLETE — report: $report_dir/ (coverage-summary.json, lcov.info, lcov-report/), payloads: $EVIDENCE_DIR/payloads/, evidence: $EVIDENCE_DIR/"
}

main "$@"
