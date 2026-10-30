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
#      ($EVIDENCE_ROOT/web-sessions/coverage-assertions.json) and the File
#      Explorer block of commands_review writes
#      $EVIDENCE_ROOT/web-commands-review/coverage-assertions.json
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
#      steering/xss/fallback drivers AND the sessions, session-restore,
#      scroll, projects, workflow-DAG (sessions_workflow), Files-drawer
#      latency (files_latency), external-sessions, presentation, loop_goal,
#      and File Explorer lanes; pointer-publishing lanes (external_sessions,
#      sessions_workflow, files_latency, session_restore, commands_review)
#      are resolved through their immutable run-dir + current.json pointer
#      so the gate always reads the exact published ledger
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
    python3 "$WEB_DIST_FINGERPRINT" stamp "$WEB_DIR" "$COV_DIST" ||
        fail "coverage: failed to fingerprint coverage bundle"
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

# Resolve a lane's executed-assertion ledger through its published pointer:
# $EVIDENCE_ROOT/<lane-root>/current.json names a run directory (relative to
# the lane's evidence root) and the ledger file inside it. Lanes publish
# set-atomically as ONE immutable run dir + ONE current.json pointer, so the
# matrix is always gated against the exact ledger of the run the pointer
# names — never a stale or hand-copied file. The pointer is validated for
# its EXACT lane-specific top-level key set (web-sessions-workflow and
# web-files-latency publish lane/nonce/run/manifest/coverage/artifacts;
# web-external-sessions publishes lane/nonce/run/manifests/coverage/phases
# with the exact per-phase {phase, manifest, documented, executed} schema) —
# any extra or missing top-level key is schema drift and fails — plus a
# non-empty nonce, a normalized `runs/`-relative run name, and the
# manifest(s) + coverage ledger + single-lane artifacts names as normalized
# contained relative paths, with the declared manifest and coverage ledger
# REQUIRED to appear inside the single-lane artifacts array (the pointer's
# names and its artifact set are one consistent publication).
# and the run dir is CANONICALIZED and CONFINED: the run directory is
# realpath-resolved and must stay inside the realpath-resolved evidence root
# (no `..` escapes, no symlinked escapes). Fails hard when the pointer is
# malformed, when the run dir/manifests/ledger is missing or not a regular
# file, or when the run escapes the evidence root (a lane that did not
# publish is a failure, not a skip).
pointer_ledger() { # <lane-evidence-dir> <lane>
    local dir="$1" lane="$2"
    [ -f "$dir/current.json" ] || fail "coverage: $lane evidence pointer missing: $dir/current.json"
    node -e '
        const fs = require("node:fs");
        const path = require("node:path");
        const [dir, lane] = process.argv.slice(1);
        const die = (message) => {
            console.error(`coverage: ${lane} pointer validation FAILED: ${message}`);
            process.exit(1);
        };
        // Every component from the filesystem root down to current.json is
        // lstat-ed and ANY symlink — including one pointing at a real file —
        // rejects the pointer outright; the pointer itself must be a regular
        // file.
        const pointerAbs = path.resolve(path.join(dir, "current.json"));
        const parts = pointerAbs.split(path.sep).filter((part) => part !== "");
        const componentStat = (prefix) => {
            try {
                return fs.lstatSync(prefix);
            } catch (err) {
                die(`pointer path component missing: ${prefix}: ${err.message}`);
            }
        };
        for (let i = 0; i < parts.length; i++) {
            const prefix = `/${parts.slice(0, i + 1).join("/")}`;
            const stats = componentStat(prefix);
            if (stats.isSymbolicLink()) {
                die(`pointer path component is a symlink (rejected): ${prefix}`);
            }
            if (i < parts.length - 1 && !stats.isDirectory()) {
                die(`pointer path component is not a directory: ${prefix}`);
            }
        }
        const pointerStats = componentStat(pointerAbs);
        if (!pointerStats.isFile()) {
            die(`pointer current.json is not a regular file: ${pointerAbs}`);
        }
        let pointer;
        try {
            pointer = JSON.parse(fs.readFileSync(pointerAbs, "utf8"));
        } catch (err) {
            die(`pointer is not readable JSON: ${err.message}`);
        }
        if (!pointer || typeof pointer !== "object" || Array.isArray(pointer)) {
            die("pointer is not a JSON object");
        }
        // Exact top-level pointer schema, PER LANE: a pointer carrying any
        // key outside the lane-specific published shape (or missing one) is
        // schema drift and is rejected — consumers never guess about
        // optional or future fields.
        const POINTER_SCHEMA = {
            "web-sessions-workflow": ["lane", "nonce", "run", "manifest", "coverage", "artifacts"],
            "web-files-latency": ["lane", "nonce", "run", "manifest", "coverage", "artifacts"],
            "web-external-sessions": ["lane", "nonce", "run", "manifests", "coverage", "phases"],
            "web-session-restore": ["lane", "nonce", "run", "manifest", "coverage", "artifacts"],
            "web-commands-review": ["lane", "nonce", "run", "manifest", "coverage", "artifacts"],
        };
        const allowedKeys = POINTER_SCHEMA[lane];
        if (!allowedKeys) {
            die(`no published pointer schema for lane ${JSON.stringify(lane)}`);
        }
        const pointerKeys = Object.keys(pointer).sort();
        const expectedKeys = [...allowedKeys].sort();
        const missingKeys = expectedKeys.filter((key) => !pointerKeys.includes(key));
        const extraKeys = pointerKeys.filter((key) => !expectedKeys.includes(key));
        if (missingKeys.length > 0 || extraKeys.length > 0) {
            die(`pointer top-level keys are not exactly [${expectedKeys.join(", ")}] (missing=${JSON.stringify(missingKeys)}, extras=${JSON.stringify(extraKeys)})`);
        }
        if (pointer.lane !== lane) {
            die(`pointer lane mismatch: expected ${JSON.stringify(lane)}, got ${JSON.stringify(pointer.lane)}`);
        }
        if (typeof pointer.nonce !== "string" || pointer.nonce === "") {
            die(`pointer nonce is not a non-empty string: ${JSON.stringify(pointer.nonce)}`);
        }
        // Normalized POSIX-relative contained names only: non-empty strings,
        // no backslashes, never absolute, exactly their posix.normalize form,
        // and no `.`/`..` components. `run` must additionally live under
        // `runs/`.
        const normalizedRelative = (value, label, runsPrefix) => {
            if (typeof value !== "string" || value === "") {
                die(`pointer ${label} is not a non-empty string: ${JSON.stringify(value)}`);
            }
            if (value.includes("\\")) {
                die(`pointer ${label} contains a backslash (POSIX relative only): ${JSON.stringify(value)}`);
            }
            if (path.posix.isAbsolute(value)) {
                die(`pointer ${label} is an absolute path: ${JSON.stringify(value)}`);
            }
            if (path.posix.normalize(value) !== value) {
                die(`pointer ${label} is not normalized (dot/dotdot/redundant separators): ${JSON.stringify(value)}`);
            }
            const segments = value.split("/");
            if (segments.some((segment) => segment === "." || segment === "..")) {
                die(`pointer ${label} contains dot/dotdot segments: ${JSON.stringify(value)}`);
            }
            if (runsPrefix && (segments[0] !== "runs" || segments.length < 2)) {
                die(`pointer ${label} is not a runs/-relative path: ${JSON.stringify(value)}`);
            }
            return value;
        };
        const run = normalizedRelative(pointer.run, "run", true);
        const coverage = normalizedRelative(pointer.coverage, "coverage", false);
        // Lane-specific manifest field: single-manifest lanes publish
        // `manifest` + `artifacts`; the external-sessions lane-set publishes
        // a `manifests` ARRAY plus an exact `phases` schema.
        let manifestList;
        let artifactList = null; // single-lane pointers only
        if (lane === "web-external-sessions") {
            if (!Array.isArray(pointer.manifests) || pointer.manifests.length === 0) {
                die(`pointer manifests is not a non-empty array: ${JSON.stringify(pointer.manifests)}`);
            }
            manifestList = pointer.manifests.map((name) => normalizedRelative(name, "manifest", false));
            // Exact phases schema: one entry per manifest, in published
            // phase order, each carrying EXACTLY
            // {phase, manifest, documented, executed} with matching names and
            // equal positive exact-once counts. Extras inside a phase entry
            // are schema drift and are rejected.
            const EXPECTED_PHASES = ["default", "native_only"];
            if (!Array.isArray(pointer.phases) || pointer.phases.length !== manifestList.length) {
                die(`pointer phases does not have one entry per manifest: ${JSON.stringify(pointer.phases)}`);
            }
            pointer.phases.forEach((entry, index) => {
                if (!entry || typeof entry !== "object" || Array.isArray(entry)) {
                    die(`pointer phases[${index}] is not an object: ${JSON.stringify(entry)}`);
                }
                const entryKeys = Object.keys(entry).sort();
                if (entryKeys.join(",") !== "documented,executed,manifest,phase") {
                    die(`pointer phases[${index}] keys are not exactly documented/executed/manifest/phase: ${JSON.stringify(entryKeys)}`);
                }
                if (entry.phase !== EXPECTED_PHASES[index]) {
                    die(`pointer phases[${index}] phase mismatch: expected ${JSON.stringify(EXPECTED_PHASES[index])}, got ${JSON.stringify(entry.phase)}`);
                }
                if (typeof entry.manifest !== "string" || entry.manifest === "") {
                    die(`pointer phases[${index}] manifest is not a non-empty string: ${JSON.stringify(entry.manifest)}`);
                }
                if (entry.manifest !== manifestList[index]) {
                    die(`pointer phases[${index}] manifest does not match manifests[${index}]: ${JSON.stringify(entry.manifest)} vs ${JSON.stringify(manifestList[index])}`);
                }
                if (!Number.isSafeInteger(entry.documented) || entry.documented < 1
                    || !Number.isSafeInteger(entry.executed) || entry.executed < 1
                    || entry.documented !== entry.executed) {
                    die(`pointer phases[${index}] documented/executed are not equal positive exact-once integers: ${JSON.stringify({ documented: entry.documented, executed: entry.executed })}`);
                }
            });
        } else {
            if (typeof pointer.manifest !== "string" || pointer.manifest === "") {
                die(`pointer manifest is not a non-empty string: ${JSON.stringify(pointer.manifest)}`);
            }
            manifestList = [normalizedRelative(pointer.manifest, "manifest", false)];
            if (!Array.isArray(pointer.artifacts) || pointer.artifacts.length === 0
                || pointer.artifacts.some((name) => typeof name !== "string" || name === "")) {
                die(`pointer artifacts is not a non-empty array of strings: ${JSON.stringify(pointer.artifacts)}`);
            }
            if (new Set(pointer.artifacts).size !== pointer.artifacts.length) {
                die(`pointer artifacts repeats a name: ${JSON.stringify(pointer.artifacts)}`);
            }
            // Every artifact name is a normalized contained relative path,
            // and the declared manifest + coverage ledger MUST both be
            // published inside artifacts — the pointer names and its
            // artifact set are one consistent publication, never
            // independent strings.
            const artifactNames = pointer.artifacts.map((name) => normalizedRelative(name, "artifact", false));
            if (!artifactNames.includes(manifestList[0])) {
                die(`pointer artifacts does not name the declared manifest: ${JSON.stringify(pointer.manifest)}`);
            }
            if (!artifactNames.includes(coverage)) {
                die(`pointer artifacts does not name the declared coverage ledger: ${JSON.stringify(pointer.coverage)}`);
            }
            artifactList = artifactNames;
        }
        // The run dir is realpath-resolved and must stay INSIDE the
        // realpath-resolved lane evidence root; every named manifest/ledger
        // is walked segment-by-segment under the run dir (symlinks rejected
        // at every level) and must resolve to a regular file inside the run.
        const rootReal = fs.realpathSync(dir);
        const resolveConfined = (base, rel, label) => {
            let current = base;
            const segments = rel.split("/");
            for (let i = 0; i < segments.length; i++) {
                current = path.join(current, segments[i]);
                const stats = componentStat(current);
                if (stats.isSymbolicLink()) {
                    die(`pointer ${label} component is a symlink (rejected): ${current}`);
                }
                if (i < segments.length - 1 && !stats.isDirectory()) {
                    die(`pointer ${label} path component is not a directory: ${current}`);
                }
            }
            const real = fs.realpathSync(current);
            const relCheck = path.relative(base, real);
            if (relCheck === "" || relCheck.startsWith("..") || path.isAbsolute(relCheck)) {
                die(`pointer ${label} escapes its root: ${rel} -> ${real}`);
            }
            return real;
        };
        const runReal = resolveConfined(rootReal, run, "run dir");
        if (!fs.statSync(runReal).isDirectory()) {
            die(`pointer run is not a directory: ${runReal}`);
        }
        const relRun = path.relative(rootReal, runReal);
        if (relRun === "" || relRun.startsWith("..") || path.isAbsolute(relRun)) {
            die(`pointer run escapes the evidence root: ${run} -> ${runReal}`);
        }
        // The coverage ledger + manifest(s) — and, for single-lane
        // pointers, EVERY published artifact — are walked segment-by-segment
        // under the run dir (symlinks rejected at every level), must stay
        // confined, and must resolve to regular files (missing artifacts
        // are a hard failure, never a skip).
        const confinedNames = artifactList === null
            ? [coverage, ...manifestList]
            : [coverage, ...manifestList, ...artifactList];
        for (const name of confinedNames) {
            const real = resolveConfined(runReal, name, "artifact");
            if (!fs.statSync(real).isFile()) {
                die(`pointer ${name} is not a regular file: ${real}`);
            }
        }
        process.stdout.write(path.join(runReal, coverage));
    ' "$dir" "$lane"
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
    export RPI_WEB_DEV_DIR="$COV_DIST" RPI_WEB_DIST_READY=1

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
    cat >"$root/home/.rpi/settings.json" <<EOF
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
    mkdir -p "$root/home/.rpi/agents"
    cat >"$root/home/.rpi/agents/writer.md" <<'EOF'
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
    python3 - "$root/home/.rpi/settings.json" "$port" <<'PY'
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
    # The real lanes write their own executed-assertion evidence during the
    # step-5 suite; every pointer-publishing lane (external_sessions,
    # sessions_workflow, files_latency, session_restore, commands_review)
    # publishes ONE immutable run dir + ONE current.json pointer, so the
    # matrix always gates the EXACT ledger of the run the pointer names —
    # never a stale or hand-copied file. The remaining direct reads are the
    # two standalone coverage drivers and the lane drivers that write a
    # single canonical ledger file themselves.
    local wf_ledger fl_ledger ext_ledger sr_ledger cr_ledger
    wf_ledger="$(pointer_ledger "$EVIDENCE_ROOT/web-sessions-workflow" "web-sessions-workflow")"
    fl_ledger="$(pointer_ledger "$EVIDENCE_ROOT/web-files-latency" "web-files-latency")"
    ext_ledger="$(pointer_ledger "$EVIDENCE_ROOT/web-external_sessions" "web-external-sessions")"
    sr_ledger="$(pointer_ledger "$EVIDENCE_ROOT/web-session-restore" "web-session-restore")"
    cr_ledger="$(pointer_ledger "$EVIDENCE_ROOT/web-commands-review" "web-commands-review")"
    node "$SCRIPT_DIR/coverage_matrix.mjs" \
        --evidence "$EVIDENCE_DIR/driver-steering/coverage-assertions.json" \
        --evidence "$EVIDENCE_DIR/driver-xss/coverage-assertions.json" \
        --evidence "$EVIDENCE_DIR/driver-fallback/coverage-assertions.json" \
        --evidence "$EVIDENCE_ROOT/web-sessions/coverage-assertions.json" \
        --evidence "$sr_ledger" \
        --evidence "$EVIDENCE_ROOT/web-scroll/coverage-assertions.json" \
        --evidence "$EVIDENCE_ROOT/web-projects/coverage-assertions.json" \
        --evidence "$wf_ledger" \
        --evidence "$fl_ledger" \
        --evidence "$ext_ledger" \
        --evidence "$EVIDENCE_ROOT/web-presentation/coverage-assertions.json" \
        --evidence "$EVIDENCE_ROOT/web-loop_goal/coverage-assertions.json" \
        --evidence "$cr_ledger" \
        --evidence "$EVIDENCE_ROOT/web-runtime-matrix/coverage-assertions.json" \
        --evidence "$EVIDENCE_ROOT/web-thinking-level/coverage-assertions.json"

    # persist payloads next to the report for later inspection
    mkdir -p "$EVIDENCE_DIR/payloads"
    cp "$PAYLOADS_DIR/"*.json "$EVIDENCE_DIR/payloads/" 2>/dev/null || true

    log "coverage: COMPLETE — report: $report_dir/ (coverage-summary.json, lcov.info, lcov-report/), payloads: $EVIDENCE_DIR/payloads/, evidence: $EVIDENCE_DIR/"
}

main "$@"
