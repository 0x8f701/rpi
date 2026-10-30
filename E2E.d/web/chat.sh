#!/usr/bin/env bash
# Multi-runner /chat lifecycle E2E lane — two real isolated rpi listeners,
# real WebSocket authentication, browser-local persistence, reconnect, and
# deletion cleanup. Playwright-only hard gate; no mock transport or DOM-only
# substitute.
set -euo pipefail
SCRIPT_DIR="$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
E2E_DIR="$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd -P)"
# shellcheck source=../lib/common.sh
. "$E2E_DIR/lib/common.sh"
# shellcheck source=lib/fixture.sh
. "$SCRIPT_DIR/lib/fixture.sh"

SCENARIO="web-chat"
TOKEN_A="web-chat-runner-a-$$-$(date +%s)"
TOKEN_B="web-chat-runner-b-$$-$(date +%s)"
WRONG_TOKEN="web-chat-wrong-$$-$(date +%s)"
CHAT_RUNNER_HANDLES=()
# Full-lane commit state: nonce-derived staging dir, the byte-exact pointer
# snapshot FILE captured before the run (compared with cmp on every failure
# path — never command substitution), and the flag that disarms the
# universal nonce-scoped EXIT/signal cleanup once the commit has succeeded.
# Ownership booleans gate every discard: cleanup removes ONLY the exact
# staging dir / run dir / pointer temp file this attempt created and still
# owns — never a prior pointer, a prior published run, or another attempt's
# temp files, and never a wildcard temp glob.
lane_nonce=""
staging=""
staging_owned=0
run_owned=0
pointer_tmp=""
pointer_tmp_owned=0
lane_pointer_before_file=""
lane_committed=0
pw_pid=0
pw_pgid=0
setup_pid=0
setup_pgid=0
root_owned=0
kill_fd=0
up_fd=0
root=""

cleanup_chat_runners() {
    local handle pid stdin_fd stdin_path
    for handle in "${CHAT_RUNNER_HANDLES[@]}"; do
        read -r pid stdin_fd stdin_path <<<"$handle"
        stop_chat_runner "$pid" "$stdin_fd" "$stdin_path"
    done
    CHAT_RUNNER_HANDLES=()
}

# Ownership-tracked discard of ONLY what this attempt created and still
# owns: the atomically-claimed staging dir, this nonce's run dir (only
# while the current pointer does not already name it — a committed run is
# durable evidence, never discarded), and the EXACT pointer temp file this
# attempt handed to the commit. Idempotent; NEVER touches prior published
# runs, the prior pointer, or any other attempt's temp files — there is NO
# wildcard temp deletion anywhere in this lane.
chat_lane_discard() {
    if [ "$staging_owned" = 1 ] && [ -n "$staging" ]; then
        rm -rf "$staging"
        staging_owned=0
    fi
    if [ "$run_owned" = 1 ] && [ -n "$lane_nonce" ] \
        && ! grep -Fq "\"nonce\": \"$lane_nonce\"" "$evidence/current.json" 2>/dev/null; then
        rm -rf "$evidence/runs/web-chat-$lane_nonce"
    fi
    run_owned=0
    if [ "$pointer_tmp_owned" = 1 ] && [ -n "$pointer_tmp" ]; then
        rm -f "$pointer_tmp"
        pointer_tmp_owned=0
    fi
}

# Bounded teardown of a setsid producer GROUP, split by ownership phase:
# while the leader is still owned (pid>0) the group is terminated and the
# leader reaped; the group is drained immediately after each leader wait,
# so no stale PGID survives — a possibly-reused leader PID is never signalled.
chat_terminate_pw() { # <grace_iterations>
    if [ "${pw_pid:-0}" -gt 0 ] 2>/dev/null; then
        terminate_registered_pid "$pw_pid" "$pw_pid" "$1"
    elif [ "${pw_pgid:-0}" -gt 0 ]; then
        terminate_and_reap 0 "$pw_pgid" "$1"
    fi
    pw_pid=0
    pw_pgid=0
}
chat_terminate_setup() { # <grace_iterations>
    if [ "${setup_pid:-0}" -gt 0 ] 2>/dev/null; then
        terminate_registered_pid "$setup_pid" "$setup_pid" "$1"
    elif [ "${setup_pgid:-0}" -gt 0 ]; then
        terminate_and_reap 0 "$setup_pgid" "$1"
    fi
    setup_pid=0
    setup_pgid=0
}
# Opt-in self-check (RPI_CHAT_CLEANUP_SELFCHECK=1), deterministic: ordinary
# child, TERM-ignoring child and group, exited leader + live group member
# (group-only mode), stale PGID re-probe, and mock pidfile identity/order.
# Only ever signals PIDs/PGIDs it just created.
chat_cleanup_selfcheck() {
    local pid pgid n tmp t0 orph state
    tmp="$WORK_ROOT/cleanup-selfcheck-$$"
    rm -rf "$tmp"
    mkdir -p "$tmp"
    sleep 30 & pid=$!
    register_pid "$pid"
    terminate_and_reap "$pid" 0 10
    kill -0 "$pid" 2>/dev/null && fail "$SCENARIO: cleanup selfcheck: ordinary child survived"
    bash -c 'trap "" TERM; : >"$1/child.ready"; exec sleep 30' _ "$tmp" & pid=$!
    register_pid "$pid"
    n=0
    while [ ! -f "$tmp/child.ready" ] && [ "$n" -lt 50 ]; do sleep 0.1; n=$((n + 1)); done
    [ -f "$tmp/child.ready" ] \
        || fail "$SCENARIO: cleanup selfcheck: TERM-ignoring child never became ready"
    terminate_and_reap "$pid" 0 5
    kill -0 "$pid" 2>/dev/null && fail "$SCENARIO: cleanup selfcheck: TERM-ignoring child survived"
    setsid bash -c 'trap "" TERM; : >"$1/group.ready"; sleep 30' _ "$tmp" & pid=$!
    register_pid "$pid"
    n=0
    while [ ! -f "$tmp/group.ready" ] && [ "$n" -lt 50 ]; do sleep 0.1; n=$((n + 1)); done
    [ -f "$tmp/group.ready" ] \
        || fail "$SCENARIO: cleanup selfcheck: TERM-ignoring group never became ready"
    terminate_and_reap "$pid" "$pid" 5
    kill -0 "$pid" 2>/dev/null \
        && fail "$SCENARIO: cleanup selfcheck: TERM-ignoring group survived the KILL fallback"
    setsid bash -c 'sleep 30 >/dev/null 2>&1 </dev/null & echo $! >"$1/orphan.pid"' _ "$tmp" & pid=$!
    register_pid "$pid"
    pgid="$pid"
    wait "$pid" 2>/dev/null || true
    kill -0 "$pid" 2>/dev/null && fail "$SCENARIO: cleanup selfcheck: group leader still alive"
    kill -0 -- "-$pgid" 2>/dev/null || fail "$SCENARIO: cleanup selfcheck: group member died before the probe"
    terminate_and_reap 0 "$pgid" 10
    # The orphaned member is terminated when it is GONE or a ZOMBIE (its
    # reaper is init, not this shell) — the group probe alone cannot tell.
    read -r orph <"$tmp/orphan.pid"
    n=0
    while [ "$n" -lt 20 ]; do
        state="$(_e2e_proc_state "$orph")"
        case "$state" in ''|Z) break ;; esac
        sleep 0.1
        n=$((n + 1))
    done
    state="$(_e2e_proc_state "$orph")"
    case "$state" in
        ''|Z) ;;
        *) fail "$SCENARIO: cleanup selfcheck: orphaned group member survived leader exit (state=$state)" ;;
    esac
    if ! kill -0 -- "-$pgid" 2>/dev/null; then
        t0=$SECONDS
        terminate_and_reap 0 "$pgid" 10
        [ $((SECONDS - t0)) -lt 2 ] \
            || fail "$SCENARIO: cleanup selfcheck: dead group was re-probed/polled instead of a bounded no-op"
    fi
    sleep 30 & pid=$!
    register_pid "$pid"
    printf '%s %s\n' "$pid" "not-the-live-identity" >"$tmp/mock.pid"
    web_kill_mock "$tmp"
    kill -0 "$pid" 2>/dev/null \
        || fail "$SCENARIO: cleanup selfcheck: pidfile identity gate signalled a non-owned pid"
    terminate_and_reap "$pid" 0 5
    [ -f "$tmp/mock.pid" ] && fail "$SCENARIO: cleanup selfcheck: mismatched pidfile not removed"
    sleep 30 & pid=$!
    register_pid "$pid"
    # Exact identity VALUE: token must be starttime stat field 22
    # (post-pid/comm positional 20), independently re-parsed.
    token="$(pid_start_token "$pid")"
    if [ -e "/proc/$pid/stat" ]; then
        case "$token" in
            ''|*[!0-9]*) fail "$SCENARIO: cleanup selfcheck: start token is not a numeric starttime: [$token]" ;;
        esac
        [ "$token" = "$(sed "s/^.*) //" "/proc/$pid/stat" | cut -d" " -f20)" ] \
            || fail "$SCENARIO: cleanup selfcheck: start token is not starttime field 22"
        [ "$token" = "$(pid_start_token "$pid")" ] \
            || fail "$SCENARIO: cleanup selfcheck: start token is not stable for a live pid"
    fi
    if [ -n "$token" ]; then
        printf '%s %s\n' "$pid" "$token" >"$tmp/mock.pid"
        web_kill_mock "$tmp"
        kill -0 "$pid" 2>/dev/null \
            && fail "$SCENARIO: cleanup selfcheck: exact pidfile identity was not terminated"
    else
        # No /proc identity: fail closed — the pidfile holder signals nothing.
        printf '%s\n' "$pid" >"$tmp/mock.pid"
        web_kill_mock "$tmp"
        kill -0 "$pid" 2>/dev/null \
            || fail "$SCENARIO: cleanup selfcheck: empty identity was signalled instead of failing closed"
        terminate_and_reap "$pid" 0 5
    fi
    sleep 0 & pid=$!
    wait "$pid" 2>/dev/null || true
    printf '%s\n' "$pid" >"$tmp/mock.pid"
    web_kill_mock "$tmp"
    [ -f "$tmp/mock.pid" ] \
        && fail "$SCENARIO: cleanup selfcheck: stale mock pid file not removed"
    rm -rf "$tmp"
    log "$SCENARIO: cleanup selfcheck OK: ordinary child, TERM-ignoring child and group, exited leader + live group member (group-only), stale PGID no-op, pidfile identity gate and stale pidfile order"
}
# Defer HUP/INT/TERM across a claim+ownership or spawn+registration, re-deliver after.
chat_defer_signals() { # <body>
    local pending="" status=0
    trap 'pending=HUP' HUP
    trap 'pending=INT' INT
    trap 'pending=TERM' TERM
    eval "$1" || status=$?
    trap 'on_chat_lane_signal HUP' HUP
    trap 'on_chat_lane_signal INT' INT
    trap 'on_chat_lane_signal TERM' TERM
    if [ -n "$pending" ]; then
        on_chat_lane_signal "$pending"
    fi
    return "$status"
}
# Bounded HTTP probe: prints the response status, ERR on connection error,
# or TIMEOUT when the 3s request/socket deadline fires without a response
# (the port still ACCEPTED the connection — i.e. still serving). A gate
# probe can never hang the lane.
chat_http_probe() { # <url>
    node -e '
const http = require("node:http");
let done = false;
const finish = (value) => { if (!done) { done = true; process.stdout.write(value); process.exit(0); } };
const request = http.get(process.argv[1], { timeout: 3000 }, (response) => finish(String(response.statusCode)));
request.on("timeout", () => { request.destroy(); finish("TIMEOUT"); });
request.on("error", () => finish("ERR"));
' "$1" 2>/dev/null || printf 'ERR'
}

# Shared teardown for EVERY exit path (playwright failure, any gate, commit
# failure, EXIT, HUP/INT/TERM): until the commit succeeds the ownership-
# tracked discard runs; after a successful commit it is disarmed
# (lane_committed=1) and only the process/workspace teardown remains.
# Idempotent — safe from both the EXIT trap and the signal handler.
on_chat_lane_exit() {
    chat_terminate_setup 25
    chat_terminate_pw 25
    cleanup_chat_runners
    if [ -n "${root:-}" ]; then
        web_kill_mock "$root" || true
    fi
    if [ "${kill_fd:-0}" -gt 0 ] 2>/dev/null; then
        eval "exec ${kill_fd}>&-" 2>/dev/null || true
        kill_fd=0
    fi
    if [ "${up_fd:-0}" -gt 0 ] 2>/dev/null; then
        eval "exec ${up_fd}>&-" 2>/dev/null || true
        up_fd=0
    fi
    if [ "$lane_committed" != 1 ]; then
        chat_lane_discard
    fi
    cleanup_e2e
    # The nonce workspace is removed LAST — after every producer group
    # member is terminated and reaped, the runner trees are stopped, the
    # mock is dead, and this shell's FIFO fds are closed, so nothing can
    # hold a path inside it. EXACT path only, gated on this attempt's
    # successful atomic claim — never a wildcard, never another attempt's
    # workspace.
    if [ "$root_owned" = 1 ] && [ -n "$root" ]; then
        rm -rf "$root"
        root_owned=0
    fi
}

# HUP/INT/TERM handler — SEPARATE from the EXIT trap: it runs the same
# shared teardown, then EXITS NONZERO with the conventional 128+signal
# status. A signal NEVER resumes the interrupted lane, never falls through
# to a gate or a pass line, and never returns into the shell.
on_chat_lane_signal() {
    local status
    case "$1" in
        HUP) status=129 ;;
        INT) status=130 ;;
        *)   status=143 ;;
    esac
    trap '' HUP INT TERM
    trap - EXIT
    on_chat_lane_exit
    log "$SCENARIO: terminated by signal $1 (exit $status) — staged run discarded, prior pointer preserved"
    exit "$status"
}

spawn_chat_runner() {
    local root="$1" evidence="$2" mock_port="$3" listen_port="${4:-0}" tag="${5:-}"
    local stdin_path="$root/stdin$tag"
    rm -f "$stdin_path"
    mkfifo "$stdin_path"
    local stdin_fd
    exec {stdin_fd}<>"$stdin_path"
    (
        cd "$root/workspace"
        exec env -i \
            HOME="$root/home" USERPROFILE="$root/home" \
            PATH="${PATH:-/usr/bin:/bin}" LANG="${LANG:-C.UTF-8}" LC_ALL="${LC_ALL:-C.UTF-8}" \
            RPI_HOME="$root/home/.rpi" PI_OFFLINE=1 PI_SKIP_VERSION_CHECK=1 \
            RPI_WEB_DEV_DIR="${RPI_WEB_DEV_DIR:-}" \
            "$RPI_BIN" --offline --continue \
            --listen "127.0.0.1:$listen_port" --listen-plaintext \
            --listen-token-file "$root/token" \
            --model user-steering/mock --api-key user-mock-key \
            <"$stdin_path" >"$evidence/rpi$tag.stdout" 2>"$evidence/rpi$tag.stderr"
    ) &
    local pid=$!
    register_pid "$pid"
    CHAT_RUNNER_HANDLES+=("$pid $stdin_fd $stdin_path")
    printf '%s %s %s\n' "$pid" "$stdin_fd" "$stdin_path"
}

wait_chat_runner() {
    local evidence="$1" tag="${2:-}" deadline=$((SECONDS + 30))
    # Optional absolute cap ($3): the caller's ONE producer deadline may be
    # sooner than this helper's own 30s bound — the tighter deadline wins.
    if [ -n "${3:-}" ] && [ "$3" -lt "$deadline" ] 2>/dev/null; then
        deadline="$3"
    fi
    while [ "$SECONDS" -lt "$deadline" ]; do
        if [ -s "$evidence/rpi$tag.stderr" ]; then
            local banner authority
            banner="$(grep -m1 'Control plane listening on http://' "$evidence/rpi$tag.stderr" || true)"
            authority="$(printf '%s\n' "$banner" | sed -n 's/.*http:\/\/\([0-9.:]*\).*/\1/p')"
            if [ -n "$authority" ]; then
                printf 'http://%s\n' "$authority"
                return 0
            fi
        fi
        sleep 0.2
    done
    fail "$SCENARIO: listener banner not seen (tag=$tag)"
}

stop_chat_runner() {
    local pid="$1" stdin_fd="$2" stdin_path="$3"
    eval "exec ${stdin_fd}>&-" 2>/dev/null || true
    rm -f "$stdin_path"
    terminate_registered_pid "$pid" 0 15
    # Reaped: drop the exact handle/PID so teardown never signals a stale PID.
    local -a kept=()
    local handle
    for handle in "${CHAT_RUNNER_HANDLES[@]}"; do
        [ "$handle" = "$pid $stdin_fd $stdin_path" ] || kept+=("$handle")
    done
    CHAT_RUNNER_HANDLES=("${kept[@]}")
    # terminate_and_reap already unregistered the exact PID from E2E_PIDS.
}
# ---------------------------------------------------------------------------
# Setup runs in its own bounded session; the producer is THE ONE background boundary.
chat_setup_playwright() { # <work> <chrome> <deadline>
    local work="$1" chrome="$2" deadline="$3"
    if [ "${RPI_WEB_PLAYWRIGHT_READY:-0}" = "1" ]; then
        [ -n "${RPI_WEB_PLAYWRIGHT_DIR:-}" ] \
            || chat_lane_fail 1 "playwright SETUP FAILED: RPI_WEB_PLAYWRIGHT_READY requires RPI_WEB_PLAYWRIGHT_DIR"
        [ -d "$RPI_WEB_PLAYWRIGHT_DIR/node_modules/playwright" ] \
            || chat_lane_fail 1 "playwright SETUP FAILED: verified playwright tree is missing: $RPI_WEB_PLAYWRIGHT_DIR"
        if [ "$work" != "$RPI_WEB_PLAYWRIGHT_DIR" ]; then
            rm -rf "$work/node_modules"
            ln -sfn "$RPI_WEB_PLAYWRIGHT_DIR/node_modules" "$work/node_modules"
        fi
        return 0
    fi
    local pending=""
    trap 'pending=HUP' HUP
    trap 'pending=INT' INT
    trap 'pending=TERM' TERM
    (
        cd "$work"
        exec setsid bash -c '
            . "$1/lib/common.sh"
            . "$2/lib/fixture.sh"
            web_install_and_probe_playwright "$3" "$4"
        ' _ "$E2E_DIR" "$SCRIPT_DIR" "$work" "$chrome"
    ) &
    setup_pid=$!
    setup_pgid="$setup_pid"
    register_pid "$setup_pid"
    trap 'on_chat_lane_signal HUP' HUP
    trap 'on_chat_lane_signal INT' INT
    trap 'on_chat_lane_signal TERM' TERM
    if [ -n "$pending" ]; then
        on_chat_lane_signal "$pending"
    fi
    while kill -0 "$setup_pid" 2>/dev/null; do
        if [ "$(_e2e_proc_state "$setup_pid" || true)" = "Z" ]; then break; fi
        if [ "$SECONDS" -ge "$deadline" ]; then
            terminate_registered_pid "$setup_pid" "$setup_pid" 1
            chat_lane_fail 1 "playwright SETUP FAILED: install/probe exceeded the lane deadline"
        fi
        sleep 0.2
    done
    local setup_status=0
    wait "$setup_pid" || setup_status=$?
    _e2e_unregister_pid "$setup_pid"
    setup_pid=0
    if [ "${setup_pgid:-0}" -gt 0 ] 2>/dev/null; then
        terminate_and_reap 0 "$setup_pgid" 25
    fi
    setup_pgid=0
    [ "$setup_status" = 0 ] \
        || chat_lane_fail 1 "playwright SETUP FAILED (node/chromium/npm unavailable)"
}

chat_run_playwright() { # <url> <evidence> <work> <test_file> <deadline> [ENV=VAL...]
    local url="$1" pw_evidence="$2" work="$3" test_file="$4" lane_deadline="$5" chrome name
    shift 5
    local -a envs=("$@")
    require_cmd node
    chrome="$(web_playwright_chrome)"
    mkdir -p "$work"
    chat_setup_playwright "$work" "$chrome" "$lane_deadline"
    name="$(basename "$test_file")"
    cp "$test_file" "$work/$name"
    if [ -f "$WEB_FIXTURE_DIR/session-lifecycle.mjs" ]; then
        mkdir -p "$work/lib"
        cp "$WEB_FIXTURE_DIR/session-lifecycle.mjs" "$work/lib/session-lifecycle.mjs"
    fi
    local -a node_preload=()
    if [ -n "${RPI_COVERAGE_DIR:-}" ] && [ -f "$WEB_FIXTURE_DIR/coverage-hook.mjs" ]; then
        node_preload+=(--import "$WEB_FIXTURE_DIR/coverage-hook.mjs")
    fi
    local pending=""
    trap 'pending=HUP' HUP
    trap 'pending=INT' INT
    trap 'pending=TERM' TERM
    (
        cd "$work"
        exec setsid env \
            "RPI_URL=$url" "RPI_TOKEN=${TOKEN:-}" \
            "RPI_CHROME=$chrome" "RPI_EVIDENCE=$pw_evidence" \
            "RPI_COVERAGE_DIR=${RPI_COVERAGE_DIR:-}" \
            "RPI_COVERAGE_LANE=${RPI_COVERAGE_LANE:-${name%.mjs}}" \
            "${envs[@]}" node "${node_preload[@]}" "$name"
    ) &
    pw_pid=$!
    pw_pgid="$pw_pid"
    register_pid "$pw_pid"
    trap 'on_chat_lane_signal HUP' HUP
    trap 'on_chat_lane_signal INT' INT
    trap 'on_chat_lane_signal TERM' TERM
    if [ -n "$pending" ]; then
        on_chat_lane_signal "$pending"
    fi
}
# ---------------------------------------------------------------------------
# Full-lane commit: Playwright stages only; this shell validates the exact
# staged set — manifest top-level keys EXACTLY lane/nonce/artifacts/
# assertions, manifest lane EXACTLY 'web-chat', manifest nonce EXACTLY the
# commit nonce (itself a strict filename token), every assertion boolean
# EXACTLY true, projectMappingsActive EXACTLY 2, registered artifact names
# unique, contained relative paths, and never the reserved manifest/backend
# filenames, the staging dir the EXACT nonce-scoped direct child of the
# evidence root, the run dir a normalized direct child of runs/, staged
# entries a regular-file-only set (lstat — symlinks rejected), nothing
# missing or extra — and then commits ONE stage-dir -> run-dir rename plus
# ONE atomically-renamed current.json pointer carrying the exact
# lane/nonce/run/manifest identity. A failure BEFORE or DURING the commit
# removes the staging dir, the renamed run dir, and the pointer temp file,
# and preserves the previous pointer byte-identical — never an early PASS
# pointer, never an orphan run dir. Fault injection (self-check only):
# dir-rename | pointer-write | pointer-rename; the manifest faults are
# exercised as ACTUAL producer failures (partial/missing write).
commit_chat_run() { # <staging> <nonce> <pointer_tmp> [faultAt: dir-rename|pointer-write|pointer-rename]
    local lane_staging="$1" lane_nonce_commit="$2" lane_pointer_tmp="$3" fault_at="${4:-}" lane_evidence
    lane_evidence="$(dirname "$lane_staging")"
    node - "$lane_staging" "$lane_nonce_commit" "$lane_evidence" "$lane_pointer_tmp" "$fault_at" <<'NODE' || return 1
const fs = require('node:fs');
const path = require('node:path');
const [staging, nonce, evidenceDir, pointerTmp, faultAt] = process.argv.slice(2);
const MANIFEST = 'playwright-assertions.json';
const BACKEND_ASSERTIONS = 'backend-assertions.json';
const POINTER = 'current.json';
const MAPPINGS_KEY = 'projectMappingsActive';
const EXPECTED_BOOLEAN_KEYS = [
  'routeMounted', 'emptyWorkspaceHero', 'emptyWorkspaceMobile', 'emptyProjectHero', 'inviteLanHint',
  'wrongTokenRejected', 'tokenTwoStepRecovered', 'twoRealRunnersConnected',
  'durableReloadRestored', 'channelSharedLog', 'channelTwoRunnersOneProject', 'channelMentionTargeted',
  'channelLogReloadRestored',
  'independentReconnect', 'samePortRecovery',
  'projectSessionIdentityIsolated', 'projectSessionCwdIsolated', 'mappingReloadRecovered',
  'deletionPersisted', 'mappedSessionCleanup', 'tokenRedaction', 'relativeLocalSubmitOnce',
  'relativeZeroRunnerCreates', 'missingDirectoryExactlyOneCreate', 'zeroBrowserErrors',
];
let renamed = false;
const runDir = path.join(evidenceDir, 'runs', `web-chat-${nonce}`);
try {
  // Nonce grammar FIRST: the nonce is a single strict filename token — it
  // names the staging dir, the run dir, and the pointer run path, so no
  // separator or traversal can ever enter a path derived from it.
  if (!/^[A-Za-z0-9][A-Za-z0-9._-]{0,127}$/.test(nonce)) {
    throw new Error(`nonce is not a strict filename token: ${JSON.stringify(nonce)}`);
  }
  const stagingName = `.staging-web-chat-${nonce}`;
  const runDirName = `runs/web-chat-${nonce}`;
  const runsDir = path.join(evidenceDir, 'runs');
  // Exact geometry: the staging dir is the nonce-scoped DIRECT child of the
  // evidence root; the run dir is a NORMALIZED, CONTAINED direct child of
  // runs/; the pointer's run path is exactly that two-segment path.
  if (path.dirname(path.resolve(staging)) !== path.resolve(evidenceDir)
    || path.basename(staging) !== stagingName) {
    throw new Error(`staging dir is not the exact nonce-scoped direct child of the evidence dir: ${staging}`);
  }
  if (path.dirname(runDir) !== runsDir || path.basename(runDir) !== `web-chat-${nonce}`
    || path.posix.normalize(runDirName) !== runDirName
    || runDirName.split('/').length !== 2 || runDirName.split('/')[0] !== 'runs') {
    throw new Error(`run dir is not a normalized, contained runs/ child: ${runDirName}`);
  }
  if (!fs.existsSync(staging)) throw new Error(`staging dir missing: ${staging}`);
  // The pointer temp file was claimed (node openSync 'wx') and is
  // ownership-tracked by the shell for failure cleanup: it must be a
  // nonce-scoped DIRECT child of the evidence dir — never a computed, never
  // a foreign path.
  if (path.dirname(path.resolve(pointerTmp)) !== path.resolve(evidenceDir)
    || !/^\.current\.json\.[A-Za-z0-9][A-Za-z0-9._-]*\.tmp$/.test(path.basename(pointerTmp))) {
    throw new Error(`pointer temp file is not a nonce-scoped direct child of the evidence dir: ${pointerTmp}`);
  }
  const manifestPath = path.join(staging, MANIFEST);
  let manifest;
  try {
    manifest = JSON.parse(fs.readFileSync(manifestPath, 'utf8'));
  } catch (error) {
    throw new Error(`staged manifest is missing or unparseable: ${error.message}`);
  }
  // Manifest schema: EXACTLY the four top-level keys lane/nonce/artifacts/
  // assertions — nothing else can ride into the published run document.
  const manifestKeys = Object.keys(manifest).sort();
  if (manifestKeys.length !== 4
    || manifestKeys.some((key, index) => key !== ['artifacts', 'assertions', 'lane', 'nonce'][index])) {
    throw new Error(`manifest top-level keys are not exactly lane/nonce/artifacts/assertions: ${JSON.stringify(Object.keys(manifest))}`);
  }
  if (manifest.lane !== 'web-chat') throw new Error(`manifest lane mismatch: ${JSON.stringify(manifest.lane)}`);
  if (manifest.nonce !== nonce) {
    throw new Error(`manifest nonce mismatch: ${JSON.stringify(manifest.nonce)} != ${JSON.stringify(nonce)}`);
  }
  const artifacts = manifest.artifacts;
  if (!Array.isArray(artifacts) || artifacts.length === 0) throw new Error('manifest carries no registered artifacts');
  if (new Set(artifacts).size !== artifacts.length) throw new Error('manifest artifact names are not unique');
  for (const name of artifacts) {
    if (typeof name !== 'string' || name === '' || name.startsWith('/') || name.includes('..')
      || name.includes('\\')) {
      throw new Error(`manifest artifact is not a contained relative path: ${JSON.stringify(name)}`);
    }
    // Reserved filenames are owned by the shell/backend, never registered
    // by the producer.
    if (name === MANIFEST || name === BACKEND_ASSERTIONS) {
      throw new Error(`manifest artifact name is reserved: ${JSON.stringify(name)}`);
    }
  }
  const assertions = manifest.assertions;
  if (typeof assertions !== 'object' || assertions === null || Array.isArray(assertions)) {
    throw new Error('manifest assertions are not an object');
  }
  const keys = Object.keys(assertions).sort();
  const expected = [...EXPECTED_BOOLEAN_KEYS, MAPPINGS_KEY].sort();
  if (keys.length !== expected.length || keys.some((key, index) => key !== expected[index])) {
    throw new Error(`manifest assertion keys drift: ${JSON.stringify(keys)}`);
  }
  for (const key of keys) {
    if (key === MAPPINGS_KEY) continue;
    if (assertions[key] !== true) {
      throw new Error(`manifest assertion ${key} is not exactly true: ${JSON.stringify(assertions[key])}`);
    }
  }
  if (assertions[MAPPINGS_KEY] !== 2) {
    throw new Error(`manifest ${MAPPINGS_KEY} is not exactly 2: ${JSON.stringify(assertions[MAPPINGS_KEY])}`);
  }
  // Exact staged set: registered artifacts + the manifest, nothing missing,
  // nothing extra, regular files only (lstat — no symlink), no nested dirs.
  const declared = new Set(artifacts);
  declared.add(MANIFEST);
  // The shell's backend RPC gate writes the backend assertions INTO the
  // staged set before the commit, so the run dir is the complete lane
  // evidence set.
  declared.add(BACKEND_ASSERTIONS);
  const actual = new Set();
  for (const entry of fs.readdirSync(staging, { withFileTypes: true })) {
    if (!entry.isFile() || entry.isSymbolicLink()) {
      throw new Error(`staged set contains a non-regular entry: ${entry.name}`);
    }
    actual.add(entry.name);
  }
  const missing = [...declared].filter((name) => !actual.has(name));
  const extras = [...actual].filter((name) => !declared.has(name));
  if (missing.length || extras.length) {
    throw new Error(`staged set mismatch: missing=${JSON.stringify(missing)} extra=${JSON.stringify(extras)}`);
  }
  for (const name of declared) {
    const stats = fs.lstatSync(path.join(staging, name));
    if (stats.isSymbolicLink() || !stats.isFile()) {
      throw new Error(`staged artifact is not a regular file: ${name}`);
    }
  }
  fs.mkdirSync(runsDir, { recursive: true });
  // The shell pre-claimed the run destination as a RESERVED EMPTY directory
  // (atomic no-clobber mkdir before the producer started). rename(2)
  // replaces an empty target directory atomically; a NON-empty target could
  // only be a foreign/corrupt reservation and fails the rename — the run
  // destination is never clobbered.
  if (faultAt === 'dir-rename') throw new Error(`injected: ${faultAt}`);
  fs.renameSync(staging, runDir);
  renamed = true;
  if (faultAt === 'pointer-write') throw new Error(`injected: ${faultAt}`);
  const pointerDoc = { lane: 'web-chat', run: runDirName, nonce, manifest: MANIFEST };
  // The pointer temp file was ATOMICALLY claimed by the shell (node
  // openSync 'wx', O_EXCL) before the producer started, so this write can
  // never clobber another attempt's temp file.
  fs.writeFileSync(pointerTmp, `${JSON.stringify(pointerDoc, null, 2)}\n`);
  // The pointer is exact before it can become current: exactly the four
  // identity fields, each naming THIS lane/nonce/run/manifest, and the run
  // path exactly the validated normalized, contained runs/ child.
  const written = JSON.parse(fs.readFileSync(pointerTmp, 'utf8'));
  if (JSON.stringify(written) !== JSON.stringify(pointerDoc)
    || Object.keys(written).length !== 4
    || written.run !== runDirName) {
    throw new Error('pointer document is not the exact lane/nonce/run/manifest identity');
  }
  if (faultAt === 'pointer-rename') throw new Error(`injected: ${faultAt}`);
  fs.renameSync(pointerTmp, path.join(evidenceDir, POINTER));
} catch (error) {
  // Pre-pointer-commit failure removes THIS attempt's staging dir, renamed
  // run dir, and pointer temp file; the previous pointer (if any) keeps
  // naming the previous complete publication.
  fs.rmSync(staging, { recursive: true, force: true });
  if (renamed) fs.rmSync(runDir, { recursive: true, force: true });
  fs.rmSync(pointerTmp, { force: true });
  console.error(`web-chat: lane-set commit FAILED: ${error && error.message ? error.message : error}`);
  process.exit(1);
}
NODE
}

# EVERY lane failure route funnels here: the universal nonce-scoped discard
# removes the staging dir, this nonce's run dir, and any temp pointer, then
# — under INJECT_FAIL — the post-mortem proves the captured production
# pointer survived byte-identical (FILE snapshot + cmp, never command
# substitution, which strips trailing newlines and mangles bytes) and that
# only the run dir THIS attempt could have created is absent.
chat_lane_fail() { # <route_status> <message>
    local route_status="$1"; shift
    if [ -n "${root:-}" ] && [ -n "${evidence:-}" ] && [ -f "$root/playwright.out" ]; then
        mkdir -p "$evidence"
        cp "$root/playwright.out" "$evidence/playwright.out" \
            || fail "$SCENARIO: could not retain playwright.out in evidence"
    fi
    # Producer groups are bounded-terminated BEFORE the staging/run/temp discard.
    chat_terminate_setup 25
    chat_terminate_pw 25
    chat_lane_discard
    if [ "${INJECT_FAIL:-0}" = "1" ]; then
        if [ -f "$lane_pointer_before_file" ]; then
            cmp -s "$evidence/current.json" "$lane_pointer_before_file" \
                || fail "$SCENARIO: INJECT_FAIL: the captured production pointer was modified by the failed run"
        elif [ -e "$evidence/current.json" ]; then
            fail "$SCENARIO: INJECT_FAIL: the failed run created a production pointer that did not exist before"
        fi
        if [ -n "$lane_nonce" ] && [ -e "$evidence/runs/web-chat-$lane_nonce" ]; then
            fail "$SCENARIO: INJECT_FAIL: this attempt's run directory survived failure cleanup"
        fi
        if [ "$pointer_tmp_owned" = 1 ] && [ -e "$pointer_tmp" ]; then
            fail "$SCENARIO: INJECT_FAIL: this attempt's temp pointer survived failure cleanup"
        fi
        log "$SCENARIO: INJECT_FAIL self-check OK: staging/run/temp removed, production pointer and prior runs untouched (status $route_status)"
    fi
    fail "$SCENARIO: $*"
}

# Stage one publication set (the synthetic producer mirroring
# chat_test.mjs): the exact playwright manifest — lane 'web-chat', the commit
# nonce, the registered artifact list, every assertion boolean exactly true
# and projectMappingsActive exactly 2 — plus the backend assertions and the
# registered regular artifact file. Drift against the commit validator fails
# the self-check, so producer and validator cannot silently diverge.
# Variants reproduce ACTUAL producer faults:
#   valid            — the fully valid staged set
#   manifest-partial — the manifest write was interrupted mid-document
#   manifest-missing — the manifest write never happened
chat_selfcheck_stage() { # <staging> <nonce> [variant: valid|manifest-partial|manifest-missing]
    local st="$1" sc_nonce="$2" variant="${3:-valid}"
    rm -rf "$st"
    mkdir -p "$st"
    printf 'shot\n' >"$st/selfcheck-shot.png"
    printf 'backend-checks\n' >"$st/backend-assertions.json"
    if [ "$variant" = "manifest-missing" ]; then
        return 0
    fi
    node - "$st/playwright-assertions.json" "$sc_nonce" "$variant" <<'NODE'
const fs = require('node:fs');
const [file, nonce, variant] = process.argv.slice(2);
const assertions = {
  routeMounted: true, emptyWorkspaceHero: true, emptyWorkspaceMobile: true,
  emptyProjectHero: true, inviteLanHint: true, wrongTokenRejected: true, tokenTwoStepRecovered: true,
  twoRealRunnersConnected: true, durableReloadRestored: true, channelSharedLog: true,
  channelTwoRunnersOneProject: true, channelMentionTargeted: true,
  channelLogReloadRestored: true, independentReconnect: true,
  samePortRecovery: true, projectSessionIdentityIsolated: true, projectSessionCwdIsolated: true,
  mappingReloadRecovered: true, deletionPersisted: true, mappedSessionCleanup: true,
  tokenRedaction: true, relativeLocalSubmitOnce: true, relativeZeroRunnerCreates: true,
  missingDirectoryExactlyOneCreate: true, zeroBrowserErrors: true, projectMappingsActive: 2,
};
const document = `${JSON.stringify({ lane: 'web-chat', nonce, artifacts: ['selfcheck-shot.png'], assertions }, null, 2)}\n`;
if (variant === 'manifest-partial') {
  // ACTUAL producer fault: the write was interrupted — only a truncated
  // prefix of the manifest ever reached the staging set.
  fs.writeFileSync(file, document.slice(0, Math.floor(document.length / 2)));
} else {
  fs.writeFileSync(file, document);
}
NODE
}

# INJECT_FAIL-only: full publication self-check in a THROWAWAY, isolated
# evidence root under THIS attempt's nonce-scoped workspace — the production
# evidence root is never written by the self-check. Seeds a prior published
# run plus a prior pointer, then exercises EVERY failure shape:
#   producer faults — an ACTUAL partial manifest write and an ACTUAL missing
#     manifest write must both fail the commit before any rename;
#   injected faults — dir-rename, pointer-write, pointer-rename must fail AT
#     the injected point.
# Every fault must roll back with the prior pointer and prior run
# byte-identical and no new residue (no run dir, no staging dir, no temp
# pointer). A final clean commit proves the committed pointer is the EXACT
# lane/nonce/run/manifest document and the run dir is exactly the staged
# set.
chat_selfcheck_assert_clean() { # <sc_root> <sc_evidence> <prior_run> <staging> <sc_nonce> <fault> <sc_tmp>
    local sc_root="$1" sc_evidence="$2" prior_run="$3" staging="$4" sc_nonce="$5" fault="$6" sc_tmp="$7"
    cmp -s "$sc_evidence/current.json" "$sc_root/pointer.before" \
        || fail "$SCENARIO: INJECT_FAIL: $fault fault modified the prior pointer"
    diff -r "$prior_run" "$sc_root/prior-run.before" >/dev/null \
        || fail "$SCENARIO: INJECT_FAIL: $fault fault modified the prior published run"
    if [ -e "$sc_evidence/runs/web-chat-$sc_nonce" ]; then
        fail "$SCENARIO: INJECT_FAIL: $fault fault left this attempt's run dir behind"
    fi
    if [ -e "$staging" ]; then
        fail "$SCENARIO: INJECT_FAIL: $fault fault left the staging dir behind"
    fi
    if [ -e "$sc_tmp" ]; then
        fail "$SCENARIO: INJECT_FAIL: $fault fault left this attempt's temp pointer behind"
    fi
}

chat_publish_selfcheck() { # <work-root>
    local sc_root="$1/selfcheck-evidence" sc_nonce="selfcheck-$lane_nonce"
    # Idempotent entry: a previous aborted self-check can never contaminate
    # this one (cp -r into an existing prior-run.before would nest copies).
    rm -rf "$sc_root"
    local sc_evidence="$sc_root/evidence"
    local prior_run="$sc_evidence/runs/web-chat-prior-$lane_nonce"
    mkdir -p "$prior_run"
    printf 'prior publication bytes\n' >"$prior_run/prior-shot.png"
    printf '{\n  "lane": "web-chat",\n  "run": "runs/web-chat-prior-%s",\n  "nonce": "prior-%s",\n  "manifest": "playwright-assertions.json"\n}\n' \
        "$lane_nonce" "$lane_nonce" >"$sc_evidence/current.json"
    cp -r "$prior_run" "$sc_root/prior-run.before"
    cp "$sc_evidence/current.json" "$sc_root/pointer.before"
    local fault staging out sc_tmp
    staging="$sc_evidence/.staging-web-chat-$sc_nonce"
    sc_tmp="$sc_evidence/.current.json.$sc_nonce.tmp"
    # ACTUAL producer faults: a partial manifest write and a missing
    # manifest write must each fail the commit at the manifest gate, with
    # the exact rollback guarantees of any other failure.
    for fault in manifest-partial manifest-missing; do
        chat_selfcheck_stage "$staging" "$sc_nonce" "$fault"
        if out="$(commit_chat_run "$staging" "$sc_nonce" "$sc_tmp" 2>&1)"; then
            fail "$SCENARIO: INJECT_FAIL: producer fault $fault never failed the commit"
        fi
        case "$out" in
            *"staged manifest is missing or unparseable"*) ;;
            *) fail "$SCENARIO: INJECT_FAIL: $fault failed with an unexpected error: $out" ;;
        esac
        chat_selfcheck_assert_clean "$sc_root" "$sc_evidence" "$prior_run" "$staging" "$sc_nonce" "$fault" "$sc_tmp"
    done
    for fault in dir-rename pointer-write pointer-rename; do
        chat_selfcheck_stage "$staging" "$sc_nonce" valid
        if out="$(commit_chat_run "$staging" "$sc_nonce" "$sc_tmp" "$fault" 2>&1)"; then
            fail "$SCENARIO: INJECT_FAIL: injected $fault fault never fired"
        fi
        case "$out" in
            *"injected: $fault"*) ;;
            *) fail "$SCENARIO: INJECT_FAIL: $fault attempt failed before the injected fault: $out" ;;
        esac
        chat_selfcheck_assert_clean "$sc_root" "$sc_evidence" "$prior_run" "$staging" "$sc_nonce" "$fault" "$sc_tmp"
    done
    chat_selfcheck_stage "$staging" "$sc_nonce" valid
    commit_chat_run "$staging" "$sc_nonce" "$sc_tmp" \
        || fail "$SCENARIO: INJECT_FAIL: clean publication of the fully valid self-check set failed"
    [ -e "$sc_tmp" ] \
        && fail "$SCENARIO: INJECT_FAIL: clean publication left this attempt's temp pointer behind"
    printf '{\n  "lane": "web-chat",\n  "run": "runs/web-chat-%s",\n  "nonce": "%s",\n  "manifest": "playwright-assertions.json"\n}\n' \
        "$sc_nonce" "$sc_nonce" >"$sc_root/pointer.expected"
    cmp -s "$sc_evidence/current.json" "$sc_root/pointer.expected" \
        || fail "$SCENARIO: INJECT_FAIL: committed pointer is not the exact lane/nonce/run/manifest document"
    local run_dir="$sc_evidence/runs/web-chat-$sc_nonce" names
    [ -d "$run_dir" ] || fail "$SCENARIO: INJECT_FAIL: clean publication left no run dir"
    names="$(node - "$run_dir" <<'NODE'
const fs = require('node:fs');
const path = require('node:path');
const dir = process.argv[2];
const names = fs.readdirSync(dir, { withFileTypes: true })
  .filter((entry) => entry.isFile() && !entry.isSymbolicLink() && fs.lstatSync(path.join(dir, entry.name)).isFile())
  .map((entry) => entry.name)
  .sort()
  .join(' ');
process.stdout.write(`${names} `);
NODE
)"
    [ "$names" = "backend-assertions.json playwright-assertions.json selfcheck-shot.png " ] \
        || fail "$SCENARIO: INJECT_FAIL: committed run dir is not the exact staged set: $names"
    diff -r "$prior_run" "$sc_root/prior-run.before" >/dev/null \
        || fail "$SCENARIO: INJECT_FAIL: clean publication modified the prior published run"
    rm -rf "$sc_root"
    log "$SCENARIO: INJECT_FAIL publication self-check OK: manifest-partial/manifest-missing producer faults and dir-rename/pointer-write/pointer-rename injections each rolled back with the prior pointer and prior run byte-identical; clean commit exact"
}

# Collision-resistant lane nonce: 12 secure-random bytes as lowercase hex
# plus the shell PID. Hex+pid+dash satisfies the strict filename token
# grammar the commit validator enforces, and the secure-random component
# makes a collision with any prior attempt's staging/run names practically
# impossible — no date-based reuse, no pid-only reuse across runs.
chat_lane_nonce() {
    local rand_hex=""
    if command -v openssl >/dev/null 2>&1; then
        rand_hex="$(openssl rand -hex 12 2>/dev/null || true)"
    fi
    if [ -z "$rand_hex" ]; then
        rand_hex="$(od -An -tx1 -N12 /dev/urandom 2>/dev/null | tr -d ' \n')"
    fi
    [ -n "$rand_hex" ] || fail "$SCENARIO: cannot generate a secure random lane nonce"
    printf '%s-%s\n' "$rand_hex" "$$"
}

# Absolute producer deadline tripped: kill and REAP the producer, then fail
# the lane through the standard ownership-tracked discard route. A deadline
# is an EXPLICIT lane failure — never a resume into the remaining gates and
# never a quiet continue or pass.
chat_lane_deadline_hit() { # <deadline>
    chat_terminate_pw 10
    chat_lane_fail deadline "producer deadline exceeded (SECONDS=$SECONDS >= $1) — playwright process group terminated and reaped — see ${root:-}/playwright.out"
}

main() {
    case "${1:-run}" in
        list|--list|--dry-run)
            printf '%s\n' 'web-chat - /chat durable add/auth probe/reconnect/delete against two real isolated rpi listeners with backend assertions and screenshots (PLAYWRIGHT-ONLY, hard gate; Playwright stages only — the shell commits the run dir + current pointer after ALL gates pass)'
            return 0
            ;;
        run|all) ;;
        *) fail "usage: $0 [run|list]" ;;
    esac

    require_rpi
    web_require_browser
    require_cmd setsid
    guard_e2e_roots
    mkdir -p "$EVIDENCE_ROOT" "$WORK_ROOT"
    if [ "${RPI_CHAT_CLEANUP_SELFCHECK:-0}" = "1" ]; then
        chat_cleanup_selfcheck
    fi

    # `root` is deliberately GLOBAL (declared at the top of the script):
    # on_chat_lane_exit runs from the EXIT/signal traps after main() has
    # returned, so the nonce workspace path must remain visible to it for
    # the final exact-path removal.
    local evidence root_a root_b evidence_a evidence_b mock_port
    # Traps FIRST: from the first claim onward, every exit path — EXIT,
    # HUP/INT/TERM — funnels into the ownership-tracked teardown.
    trap on_chat_lane_exit EXIT
    trap 'on_chat_lane_signal HUP' HUP
    trap 'on_chat_lane_signal INT' INT
    trap 'on_chat_lane_signal TERM' TERM
    # The lane nonce is generated BEFORE any workspace exists: SECURE RANDOM
    # hex + PID (collision-resistant — a nonce collision with any prior
    # attempt is practically impossible), validated against the strict
    # filename token grammar the commit enforces. EVERY workspace and
    # transient path below (including the INJECT_FAIL-only self-check root)
    # is derived from this nonce, so two concurrent attempts of this
    # scenario never share — and never delete — a workspace, FIFO, handle,
    # log, or self-check path. Playwright STAGES ONLY into the nonce-derived
    # staging dir; the shell performs the single full-lane commit after
    # every gate below.
    lane_nonce="$(chat_lane_nonce)"
    if [ "${#lane_nonce}" -gt 128 ] \
        || [[ ! "$lane_nonce" =~ ^[A-Za-z0-9][A-Za-z0-9._-]{0,127}$ ]]; then
        fail "$SCENARIO: lane nonce is not a strict filename token: $lane_nonce"
    fi
    root="$WORK_ROOT/web-chat-$lane_nonce"
    evidence="$EVIDENCE_ROOT/$SCENARIO"
    # ATOMIC nonce-scoped workspace claim: a plain mkdir (no -p, and NEVER a
    # pre-existing rm -rf) fails when the exact workspace already exists —
    # this attempt never deletes or reuses another attempt's workspace,
    # replacing the shared scenario_workspace scheme.
    chat_defer_signals 'mkdir "$root" && root_owned=1' \
        || fail "$SCENARIO: atomic workspace claim failed for nonce $lane_nonce (existing workspace rejected)"
    register_cleanup_path "$root"
    root_a="$root/runner-a"
    root_b="$root/runner-b"
    mkdir -p "$root/home/.rpi" "$root/workspace" "$root/evidence" "$evidence"
    # Runner stdout/stderr and the playwright transcript are TRANSIENT
    # per-attempt diagnostics: they live under the nonce-scoped workspace,
    # never under the shared production evidence root.
    evidence_a="$root/evidence/runner-a"
    evidence_b="$root/evidence/runner-b"
    mkdir -p "$evidence/runs"
    # ATOMIC staging claim: plain mkdir (no -p) fails when the exact
    # nonce-scoped staging dir already exists, so a colliding attempt is
    # REJECTED — never staged into, never later discarded by this attempt.
    staging="$evidence/.staging-web-chat-$lane_nonce"
    chat_defer_signals 'mkdir "$staging" && staging_owned=1' \
        || fail "$SCENARIO: atomic staging claim failed for nonce $lane_nonce (existing staging dir rejected)"
    # ATOMIC run-destination claim: a RESERVED EMPTY directory created with
    # a plain no-clobber mkdir BEFORE anything is spawned. The commit's
    # staging->run rename replaces exactly this empty reservation atomically;
    # a non-empty reservation can only be a collision and is rejected here.
    chat_defer_signals 'mkdir "$evidence/runs/web-chat-$lane_nonce" && run_owned=1' \
        || fail "$SCENARIO: atomic run-destination claim failed for nonce $lane_nonce (collision rejected)"
    pointer_tmp="$evidence/.current.json.$lane_nonce.tmp"
    # ATOMIC no-clobber pointer-temp claim via O_EXCL (node open 'wx'): the
    # exact nonce-scoped temp path is claimed before anything is spawned, so
    # the commit can never clobber another attempt's temp file, and cleanup
    # ownership begins only from this successful claim.
    chat_defer_signals 'node -e '\''const fs = require("node:fs"); fs.closeSync(fs.openSync(process.argv[1], "wx"));'\'' "$pointer_tmp" 2>/dev/null && pointer_tmp_owned=1' \
        || fail "$SCENARIO: atomic pointer-temp claim failed for nonce $lane_nonce (existing temp file rejected)"
    # Byte-EXACT snapshot of the production pointer BEFORE this attempt, via
    # file copy (empty-file marker absent when no pointer exists yet): every
    # failure path compares with cmp -s, never command substitution.
    lane_pointer_before_file="$root/pointer.before"
    if [ -f "$evidence/current.json" ]; then
        cp "$evidence/current.json" "$lane_pointer_before_file"
    else
        rm -f "$lane_pointer_before_file"
    fi
    # INJECT_FAIL-only: full publication self-check in a THROWAWAY evidence
    # root under THIS attempt's nonce-scoped workspace; the production
    # evidence root is never written by the self-check, and another
    # attempt's self-check paths can never collide with it.
    if [ "${INJECT_FAIL:-0}" = "1" ]; then
        chat_publish_selfcheck "$root"
    fi
    # ONE loopback mock provider for both runners: web_start_mock bakes its
    # port into $root/home/.rpi/models.json, so the assignment MUST be
    # restored BEFORE the models copy and every runner spawn below. Its
    # transcript is a TRANSIENT per-attempt diagnostic, so it is passed THIS
    # attempt's nonce-scoped evidence dir — never the shared production
    # evidence root.
    mock_port="$(web_start_mock "$root" "$root/evidence")"
    mkdir -p "$root_a/home/.rpi" "$root_a/workspace" "$root_a/project" \
        "$root_b/home/.rpi" "$root_b/workspace" "$root_b/project" "$evidence_a" "$evidence_b"
    printf '{}\n' >"$root_a/home/.rpi/settings.json"
    printf '{}\n' >"$root_b/home/.rpi/settings.json"
    cp "$root/home/.rpi/models.json" "$root_a/home/.rpi/models.json"
    cp "$root/home/.rpi/models.json" "$root_b/home/.rpi/models.json"
    printf '%s\n' "$TOKEN_A" >"$root_a/token"
    printf '%s\n' "$TOKEN_B" >"$root_b/token"
    printf 'runner-a-workspace\n' >"$root_a/workspace/runner-a.txt"
    local pid_a fd_a stdin_a pid_b fd_b stdin_b url_a url_b chat_url b_port
    chat_defer_signals 'spawn_chat_runner "$root_a" "$evidence_a" "$mock_port" >"$root/runner-a.handle"'
    read -r pid_a fd_a stdin_a <"$root/runner-a.handle"
    chat_defer_signals 'spawn_chat_runner "$root_b" "$evidence_b" "$mock_port" >"$root/runner-b.handle"'
    read -r pid_b fd_b stdin_b <"$root/runner-b.handle"
    url_a="$(wait_chat_runner "$evidence_a")"
    url_b="$(wait_chat_runner "$evidence_b")"
    # Restore and VALIDATE the producer surfaces IMMEDIATELY after url_b
    # readiness — they are never referenced unset, empty, or malformed below:
    #   chat_url — the /chat entry page on runner A's observed authority
    #     (runner A is never restarted, so it is the stable host for the SPA
    #     across runner B's kill/respawn);
    #   b_port   — runner B's numeric listen port, reused for the SAME-PORT
    #     respawn below.
    chat_url="${url_a%/web}/chat"
    case "$chat_url" in
        http://127.0.0.1:*/chat) ;;
        *) chat_lane_fail 3 "malformed chat URL derived from runner A banner: $url_a" ;;
    esac
    b_port="$(printf '%s' "$url_b" | sed -n 's#^http://[^/:]*:\([0-9][0-9]*\)$#\1#p')"
    case "$b_port" in
        ''|*[!0-9]*) chat_lane_fail 3 "could not parse a numeric runner B port from: $url_b" ;;
    esac
    [ "$b_port" -gt 0 ] \
        || chat_lane_fail 3 "runner B port is not positive: $b_port (from $url_b)"
    log "$SCENARIO: runner A at $url_a; runner B at $url_b (respawn port $b_port); chat at $chat_url"
    # Kill/respawn is EVENT-DRIVEN: two FIFOs exist BEFORE the driver starts
    # (and stay open rw in this shell), so the kill request and the
    # respawned-up signal never depend on file-polling races. This shell
    # BLOCKS on the kill-request FIFO while Playwright is alive — there is
    # no short deadline that could silently skip the kill/respawn and turn
    # the lane into a false failure.
    mkdir -p "$root/playwright"
    local kill_fifo="$root/playwright/kill-runner-b.fifo"
    local up_fifo="$root/playwright/runner-b-up.fifo"
    rm -f "$kill_fifo" "$up_fifo"
    mkfifo "$kill_fifo" "$up_fifo"
    # kill_fd/up_fd are GLOBAL (top of the script): the EXIT/signal
    # teardown closes them before the exact workspace removal.
    local kill_line
    exec {kill_fd}<>"$kill_fifo" {up_fd}<>"$up_fifo"

    local pw_status=0
    local lane_deadline=$((SECONDS + 900))
    chat_run_playwright "$chat_url" "$root/evidence" "$root/playwright" "$SCRIPT_DIR/chat_test.mjs" "$lane_deadline" \
        "RPI_RUNNER_A_ORIGIN=${url_a%/web}" \
        "RPI_RUNNER_B_ORIGIN=${url_b%/web}" \
        "RPI_RUNNER_A_TOKEN=$TOKEN_A" \
        "RPI_RUNNER_B_TOKEN=$TOKEN_B" \
        "RPI_WRONG_TOKEN=$WRONG_TOKEN" \
        "RPI_RUNNER_A_CWD=$root_a/workspace" \
        "RPI_RUNNER_B_CWD=$root_b/workspace" \
        "RPI_RUNNER_A_PROJECT=$root_a/project" \
        "RPI_RUNNER_B_PROJECT=$root_b/project" \
        "RPI_LANE_NONCE=$lane_nonce" \
        "RPI_RUNNER_A_HOME=$root_a/home/.rpi" \
        "RPI_RUNNER_B_HOME=$root_b/home/.rpi" \
        "RPI_WORK=$root/playwright" \
        "RPI_STAGING=$staging" \
        "INJECT_FAIL=${INJECT_FAIL:-0}" >"$root/playwright.out" 2>&1
    local restored_b fifo_wait
    while kill -0 "$pw_pid" 2>/dev/null; do
        if [ "$(_e2e_proc_state "$pw_pid" || true)" = "Z" ]; then break; fi
        if [ "$SECONDS" -ge "$lane_deadline" ]; then
            chat_lane_deadline_hit "$lane_deadline"
        fi
        fifo_wait=$((lane_deadline - SECONDS))
        [ "$fifo_wait" -gt 5 ] && fifo_wait=5
        if [ "$fifo_wait" -le 0 ]; then
            chat_lane_deadline_hit "$lane_deadline"
        fi
        if read -r -t "$fifo_wait" -u "$kill_fd" kill_line; then
            log "$SCENARIO: kill request received; killing runner B on port $b_port"
            stop_chat_runner "$pid_b" "$fd_b" "$stdin_b"
            if [ "$SECONDS" -ge "$lane_deadline" ]; then
                chat_lane_deadline_hit "$lane_deadline"
            fi
            chat_defer_signals 'spawn_chat_runner "$root_b" "$evidence_b" "$mock_port" "$b_port" "2" >"$root/runner-b2.handle"'
            read -r pid_b fd_b stdin_b <"$root/runner-b2.handle"
            restored_b="$(wait_chat_runner "$evidence_b" "2" "$lane_deadline")"
            [ "$restored_b" = "$url_b" ] \
                || chat_lane_fail 4 "runner B authority changed after restart (expected $url_b, got $restored_b)"
            if [ "$SECONDS" -ge "$lane_deadline" ]; then
                chat_lane_deadline_hit "$lane_deadline"
            fi
            printf 'runner-b-up\n' >&"$up_fd"
        fi
    done
    wait "$pw_pid" || pw_status=$?
    _e2e_unregister_pid "$pw_pid"
    pw_pid=0
    if [ "${pw_pgid:-0}" -gt 0 ] 2>/dev/null; then
        terminate_and_reap 0 "$pw_pgid" 25
    fi
    pw_pgid=0
    if [ "$pw_status" -eq 0 ]; then
        # Playwright STAGED only — nothing is published yet. The pointer can
        # never name a run before the backend RPC, route, cleanup, and
        # listener-close gates below have passed.
        grep -q "^web-chat: STAGED $staging$" "$root/playwright.out" \
            || chat_lane_fail staged "playwright exited 0 without its STAGED marker — see $root/playwright.out"
        printf 'playwright: two-runner add/auth probe/reconnect/delete staged\n' | tee "$root/playwright-summary.txt"
    else
        chat_lane_fail "$pw_status" "playwright lane failed (exit $pw_status) — see $root/playwright.out"
    fi

    # Gate 1: backend RPC identity/cwd isolation. Failure removes the staged
    # run and preserves the prior pointer.
    node - "$url_a" "$TOKEN_A" "$root_a/workspace" "$url_b" "$TOKEN_B" "$root_b/workspace" "$staging/backend-assertions.json" <<'NODE' || chat_lane_fail 4 "backend RPC identity/cwd isolation gate failed"
const http = require('http');
const fs = require('fs');
const [urlA, tokenA, cwdA, urlB, tokenB, cwdB, output] = process.argv.slice(2);
function rpc(base, token, id) {
  return new Promise((resolve, reject) => {
    const body = JSON.stringify({ type: 'get_state', id });
    const target = new URL('/rpc', base);
    const request = http.request(target, {
      method: 'POST',
      headers: {
        authorization: `Bearer ${token}`,
        'content-type': 'application/json',
        'content-length': Buffer.byteLength(body),
      },
    }, (response) => {
      let text = '';
      response.setEncoding('utf8');
      response.on('data', (chunk) => { text += chunk; });
      response.on('end', () => resolve({ status: response.statusCode, body: JSON.parse(text) }));
    });
    request.on('error', reject);
    request.setTimeout(10000, () => request.destroy(new Error('backend RPC timed out after 10s')));
    request.end(body);
  });
}
(async () => {
  const a = await rpc(urlA, tokenA, 'chat-backend-a');
  const b = await rpc(urlB, tokenB, 'chat-backend-b');
  if (a.status !== 200 || a.body.success !== true || a.body.data?.cwd !== cwdA) {
    throw new Error(`runner A backend mismatch: ${JSON.stringify({ status: a.status, cwd: a.body.data?.cwd })}`);
  }
  if (b.status !== 200 || b.body.success !== true || b.body.data?.cwd !== cwdB) {
    throw new Error(`runner B backend mismatch: ${JSON.stringify({ status: b.status, cwd: b.body.data?.cwd })}`);
  }
  if (a.body.data?.sessionId === b.body.data?.sessionId) {
    throw new Error('runner session identities are not isolated');
  }
  fs.writeFileSync(output, JSON.stringify({
    runnerA: { status: a.status, cwdMatched: true, sessionBound: Boolean(a.body.data?.sessionId) },
    runnerB: { status: b.status, cwdMatched: true, sessionBound: Boolean(b.body.data?.sessionId) },
    isolatedSessionIds: true,
  }, null, 2));
})().catch((error) => { console.error(error.message); process.exit(2); });
NODE
    # Gate 2: route surface (bounded probes — a stalled socket can never
    # hang the gate).
    local chat_status unsupported_status
    chat_status="$(chat_http_probe "$chat_url")"
    unsupported_status="$(chat_http_probe "${url_a%/web}/chat-missing")"
    if [ "$chat_status" != "200" ] || [ "$unsupported_status" != "404" ]; then
        chat_lane_fail 5 "route gate failed (chat=$chat_status unsupported=$unsupported_status) — staged run discarded, prior pointer preserved"
    fi

    # Gate 3: cleanup. Runners and the mock provider stop before any commit.
    cleanup_chat_runners
    web_kill_mock "$root"

    # Gate 4: listener close sanity. The cleanup gate must have ACTUALLY
    # closed every fixture listener: both runner control planes and the mock
    # provider refuse connections before anything is committed. The probe is
    # bounded (3s request/socket deadline) — a stalled socket reports
    # TIMEOUT, which still means the port is accepting and the gate FAILS.
    local close_target close_status
    for close_target in "$url_a" "$url_b" "http://127.0.0.1:$mock_port"; do
        close_status="$(chat_http_probe "$close_target")"
        [ "$close_status" = "ERR" ] \
            || chat_lane_fail 6 "listener close sanity failed ($close_target still serving status=$close_status) — commit withheld"
    done

    # Gate 5: the single full-lane commit. web_finish_lane is NOT used here —
    # its close half is already owned by the cleanup gate (stop_chat_runner
    # closes each runner stdin; nothing holds the fixture ports open), and its
    # PASS line must never print before the commit: the lane PASS line is
    # printed below ONLY after commit_chat_run succeeds.
    # The staging dir, the reserved run destination, and the pointer temp
    # file were all ATOMICALLY claimed BEFORE the producer started and are
    # owned since their claims, so a signal landing anywhere before the
    # commit discards exactly the paths this attempt owns.
    chat_defer_signals 'commit_chat_run "$staging" "$lane_nonce" "$pointer_tmp" && lane_committed=1' \
        || chat_lane_fail 7 "lane-set commit failed — staging/run/temp removed, prior pointer preserved"
    # Commit succeeded: the published run dir and pointer are durable lane
    # evidence; the nonce-scoped EXIT/signal discard is disarmed
    # (lane_committed=1) and only teardown plus the PASS line remain.
    log "$SCENARIO: published $evidence/runs/web-chat-$lane_nonce via $evidence/current.json"
    printf '%s passed\nevidence=%s\n' "$SCENARIO" "$EVIDENCE_ROOT/$SCENARIO"
}
main "$@"
