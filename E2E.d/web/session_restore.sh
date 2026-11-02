#!/usr/bin/env bash
# Web session restore + persistence E2E lane (playwright-only, hard-fail).
#
# Drives the real `rpi --listen` Web-only binary + loopback mock (scenario
# `sessions`, instant content-routed replies) through a real Playwright
# browser and asserts (see session_restore_test.mjs):
#   R0  a persisted fixture restores legitimate assistant/IRC content, paints
#       stored [text, thinking] thinking-first, hides
#       display:false internal customs, and bounds bash/tool output
#   R1  a Web prompt round-trips and the new session's row appears
#   R2  switching away/back to a LOADED session restores its transcript
#       from the authoritative backend snapshot
#   R4  after SIGTERM-restarting the listener, the Web auto-reconnects without
#       reload and a post-reconnect switch restores disk-backed history
#   R5  after a page reload, the fresh load boots the SERVING page authority
#       (never a persisted host value) and restores the PAGE-authority saved
#       session, while the session chosen through a host-switch cutover stays
#       stored under the connection authority's own key (per-authority
#       preference isolation)
#   R6  a saved listener preference naming a nonexistent session falls back
#       to the first catalog row, which becomes active with its transcript
#
# The lane FAILS (non-zero) when playwright/chromium cannot be used; there
# is no agent-browser fallback and no skip.
#
# Evidence publication is failure-clean and set-atomic: the playwright
# driver STAGES ONLY (screenshots, coverage-assertions.json, the assertions
# manifest) into the ONE run-scoped staging dir the shell atomically claimed
# before the producer started. After the producer exits 0 with its STAGED
# marker, the shell runs the final gates FIRST — web_sanity_http plus the
# close/listener gate (stdin EOF must stop the listener) — and only then
# performs THE sole commit: it validates the exact staged set/ledger
# (manifest lane/nonce/artifacts schema, unique contained regular-file
# artifact names, nothing missing or extra, documented AND executed ledger
# arrays equal elementwise with every count exactly 1), commits with ONE
# stage-dir -> immutable run-dir rename (runs/web-session-restore-<nonce>/)
# plus ONE atomically-renamed current.json pointer, and ONLY THEN prints the
# lane's sole PASS line. EVERY step runs inside ONE rollback scope: any
# validation failure or fault removes the staging dir/new run/temp and
# preserves the prior pointer byte-identical; the invocation trap removes
# this attempt's staging set / run dir / pointer temp on ANY setup, producer,
# or final-gate failure.

#
# Usage: bash E2E.d/web/session_restore.sh [run|list]
set -euo pipefail
SCRIPT_DIR="$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
E2E_DIR="$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd -P)"
# shellcheck source=../lib/common.sh
. "$E2E_DIR/lib/common.sh"
# shellcheck source=lib/fixture.sh
. "$SCRIPT_DIR/lib/fixture.sh"

SCENARIO="web-session-restore"
TOKEN="web-session-restore-e2e-token-$$-$(date +%s)"
MOCK_SCENARIO="sessions"

# Full-lane publication state: the secure-random lane nonce derives the
# staging dir, the reserved run destination, and the pointer temp file, all
# ATOMICALLY claimed BEFORE the producer starts; the byte-exact pointer
# snapshot FILE (compared with cmp on every failure path — never command
# substitution) and the flag disarming the nonce-scoped EXIT/signal discard
# once the sole commit succeeded. root/evidence are top-level so the traps
# always see them. Ownership booleans gate every discard: cleanup removes
# ONLY the exact staging dir / run dir / pointer temp file this attempt
# created and still owns — never a prior pointer, a prior published run, or
# another attempt's temp files.
root="" evidence=""
lane_nonce="" staging="" staging_owned=0 run_owned=0
pointer_tmp="" pointer_tmp_owned=0
lane_pointer_before_file="" lane_committed=0 pw_pid=0

sr_lane_discard() {
    if [ "$staging_owned" = 1 ] && [ -n "$staging" ]; then
        rm -rf "$staging"
        staging_owned=0
    fi
    if [ "$run_owned" = 1 ] && [ -n "$lane_nonce" ] \
        && ! grep -Fq "\"nonce\": \"$lane_nonce\"" "$evidence/current.json" 2>/dev/null; then
        rm -rf "$evidence/runs/web-session-restore-$lane_nonce"
    fi
    run_owned=0
    if [ "$pointer_tmp_owned" = 1 ] && [ -n "$pointer_tmp" ]; then
        rm -f "$pointer_tmp"
        pointer_tmp_owned=0
    fi
}

# Shared teardown for EVERY exit path until the commit succeeds; after it,
# only the process/workspace teardown remains. Idempotent.
on_sr_lane_exit() {
    if [ "$lane_committed" != 1 ]; then
        sr_lane_discard
    fi
    if [ -n "${root:-}" ]; then
        web_kill_mock "$root" || true
    fi
    cleanup_e2e
}

# HUP/INT/TERM: the same teardown, then a NONZERO 128+signal exit — a signal
# never resumes the lane into a gate or a pass line.
on_sr_lane_signal() {
    local status
    case "$1" in
        HUP) status=129 ;;
        INT) status=130 ;;
        *)   status=143 ;;
    esac
    trap '' HUP INT TERM
    trap - EXIT
    on_sr_lane_exit
    log "$SCENARIO: terminated by signal $1 (exit $status) — staged run discarded, prior pointer preserved"
    exit "$status"
}

# Collision-resistant lane nonce: 12 secure-random bytes as lowercase hex
# plus the shell PID — satisfies the strict filename token grammar the commit
# validator enforces.
sr_lane_nonce() {
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

# ---------------------------------------------------------------------------
# Full-lane commit: the driver STAGED ONLY; this shell validates the exact
# staged set — manifest top-level keys EXACTLY lane/nonce/artifacts, manifest
# lane EXACTLY 'web-session-restore', nonce EXACTLY the commit nonce (a
# strict filename token), registered artifact names unique contained relative
# paths (never the reserved manifest filename), the coverage ledger named,
# the staging dir the EXACT nonce-scoped direct child of the evidence root,
# the run dir a normalized direct child of runs/, staged entries a
# regular-file-only set (lstat — symlinks rejected) with nothing missing or
# extra, and the ledger's documented AND executed arrays equal to the
# documented ID order elementwise with every count exactly 1 — then commits
# ONE stage-dir -> run-dir rename plus ONE atomically-renamed current.json
# pointer (lane/nonce/run/manifest/coverage/artifacts). A failure BEFORE or
# DURING the commit removes the staging dir, the renamed run dir, and the
# pointer temp file, and preserves the previous pointer byte-identical —
# never an early PASS pointer, never an orphan run dir.
commit_sr_run() { # <staging> <nonce> <pointer_tmp>
    local lane_staging="$1" lane_nonce_commit="$2" lane_pointer_tmp="$3" lane_evidence
    lane_evidence="$(dirname "$lane_staging")"
    node - "$lane_staging" "$lane_nonce_commit" "$lane_evidence" "$lane_pointer_tmp" <<'NODE' || return 1
const fs = require('node:fs');
const path = require('node:path');
const [staging, nonce, evidenceDir, pointerTmp] = process.argv.slice(2);
const LANE = 'web-session-restore';
const MANIFEST = 'playwright-assertions.json';
const LEDGER = 'coverage-assertions.json';
const EXPECTED_IDS = [
  'R5.1', 'R5.2', 'R5.3',
  'R6.1', 'R6.2', 'R6.3',
  'R4.1', 'R4.2', 'R4.3', 'R4.4', 'R4.5',
];
let renamed = false;
const runDirName = `runs/${LANE}-${nonce}`;
const runDir = path.join(evidenceDir, runDirName);
try {
  if (!/^[A-Za-z0-9][A-Za-z0-9._-]{0,127}$/.test(nonce)) {
    throw new Error(`nonce is not a strict filename token: ${JSON.stringify(nonce)}`);
  }
  if (path.dirname(path.resolve(staging)) !== path.resolve(evidenceDir)
    || path.basename(staging) !== `.staging-${LANE}-${nonce}`) {
    throw new Error(`staging dir is not the exact nonce-scoped direct child of the evidence dir: ${staging}`);
  }
  if (path.dirname(runDir) !== path.join(evidenceDir, 'runs')
    || path.basename(runDir) !== `${LANE}-${nonce}`
    || path.posix.normalize(runDirName) !== runDirName
    || runDirName.split('/').length !== 2 || runDirName.split('/')[0] !== 'runs') {
    throw new Error(`run dir is not a normalized, contained runs/ child: ${runDirName}`);
  }
  if (path.dirname(path.resolve(pointerTmp)) !== path.resolve(evidenceDir)
    || !/^\.current\.json\.[A-Za-z0-9][A-Za-z0-9._-]{0,127}\.tmp$/.test(path.basename(pointerTmp))) {
    throw new Error(`pointer temp file is not a nonce-scoped direct child of the evidence dir: ${pointerTmp}`);
  }
  if (!fs.existsSync(staging)) throw new Error(`staging dir missing: ${staging}`);
  let manifest;
  try {
    manifest = JSON.parse(fs.readFileSync(path.join(staging, MANIFEST), 'utf8'));
  } catch (error) {
    throw new Error(`staged manifest is missing or unparseable: ${error.message}`);
  }
  const manifestKeys = Object.keys(manifest).sort();
  if (manifestKeys.length !== 3
    || manifestKeys.some((key, index) => key !== ['artifacts', 'lane', 'nonce'][index])) {
    throw new Error(`manifest top-level keys are not exactly lane/nonce/artifacts: ${JSON.stringify(Object.keys(manifest))}`);
  }
  if (manifest.lane !== LANE) throw new Error(`manifest lane mismatch: ${JSON.stringify(manifest.lane)}`);
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
    if (name === MANIFEST) {
      throw new Error(`manifest artifact name is reserved: ${JSON.stringify(name)}`);
    }
  }
  if (!artifacts.includes(LEDGER)) {
    throw new Error(`manifest artifacts does not name the coverage ledger: ${LEDGER}`);
  }
  const declared = new Set(artifacts);
  declared.add(MANIFEST);
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
  let ledger;
  try {
    ledger = JSON.parse(fs.readFileSync(path.join(staging, LEDGER), 'utf8'));
  } catch (error) {
    throw new Error(`staged ledger is missing or unparseable: ${error.message}`);
  }
  const ledgerKeys = Object.keys(ledger).sort();
  if (ledgerKeys.length !== 3
    || ledgerKeys.some((key, index) => key !== ['counts', 'documented', 'executed'][index])) {
    throw new Error(`ledger top-level keys are not exactly documented/executed/counts: ${JSON.stringify(Object.keys(ledger))}`);
  }
  const arraysEqual = (a, b) => Array.isArray(a) && a.length === b.length && a.every((x, i) => x === b[i]);
  if (!arraysEqual(ledger.documented, EXPECTED_IDS)) {
    throw new Error(`ledger documented mismatch: ${JSON.stringify(ledger.documented)}`);
  }
  if (!arraysEqual(ledger.executed, EXPECTED_IDS)) {
    throw new Error(`ledger executed (actual execution order) mismatch: ${JSON.stringify(ledger.executed)}`);
  }
  const counts = ledger.counts;
  if (!counts || typeof counts !== 'object' || Array.isArray(counts)
    || Object.keys(counts).length !== EXPECTED_IDS.length
    || EXPECTED_IDS.some((id) => counts[id] !== 1)) {
    throw new Error(`ledger counts are not exactly-once for every documented id: ${JSON.stringify(ledger.counts)}`);
  }
  fs.mkdirSync(path.join(evidenceDir, 'runs'), { recursive: true });
  // The shell pre-claimed the run destination as a RESERVED EMPTY directory:
  // rename(2) replaces an empty target atomically; a non-empty target could
  // only be a foreign/corrupt reservation and fails the rename.
  fs.renameSync(staging, runDir);
  renamed = true;
  const pointerDoc = {
    lane: LANE,
    nonce,
    run: runDirName,
    manifest: MANIFEST,
    coverage: LEDGER,
    artifacts: [...declared].sort(),
  };
  fs.writeFileSync(pointerTmp, `${JSON.stringify(pointerDoc, null, 2)}\n`);
  const written = JSON.parse(fs.readFileSync(pointerTmp, 'utf8'));
  if (JSON.stringify(written) !== JSON.stringify(pointerDoc)
    || Object.keys(written).length !== 6 || written.run !== runDirName) {
    throw new Error('pointer document is not the exact lane/nonce/run/manifest/coverage/artifacts identity');
  }
  fs.renameSync(pointerTmp, path.join(evidenceDir, 'current.json'));
} catch (error) {
  // Pre-pointer-commit failure removes THIS attempt's staging dir, renamed
  // run dir, and pointer temp file; the previous pointer (if any) keeps
  // naming the previous complete publication.
  fs.rmSync(staging, { recursive: true, force: true });
  if (renamed) fs.rmSync(runDir, { recursive: true, force: true });
  fs.rmSync(pointerTmp, { force: true });
  console.error(`${LANE}: publication commit FAILED: ${error && error.message ? error.message : error}`);
  process.exit(1);
}
NODE
}

# ---------------------------------------------------------------------------
main() {
    case "${1:-run}" in
        list|--list|--dry-run)
            printf '%s\n' \
                'web-session-restore - session parity: prompt recorded, loaded switch restore, exact SIGTERM reconnect/switch restore with switch correlation, page-authority preference reload with per-authority isolation, missing-preference fallback (PLAYWRIGHT-ONLY, hard-fail)'
            return 0
            ;;
        run|all) ;;
        *) fail "usage: $0 [run|list]" ;;
    esac

    require_rpi
    web_require_browser

    local port url listen_port pw_status=0 pw_pid=0
    root="$(scenario_workspace "$SCENARIO")"
    evidence="$EVIDENCE_ROOT/$SCENARIO"
    session_dir="$root/sessions"
    python3 - "$root/home/.rpi/settings.json" "$session_dir" <<'PY'
import json, sys
open(sys.argv[1], 'w', encoding='utf-8').write(json.dumps({'sessionDir': sys.argv[2]}) + '\n')
PY
    printf '%s\n' "$TOKEN" >"$root/token"
    # Traps FIRST: from the first claim onward, every exit path — EXIT,
    # HUP/INT/TERM — funnels into the ownership-tracked teardown.
    trap on_sr_lane_exit EXIT
    trap 'on_sr_lane_signal HUP' HUP
    trap 'on_sr_lane_signal INT' INT
    trap 'on_sr_lane_signal TERM' TERM
    lane_nonce="$(sr_lane_nonce)"
    # ATOMIC claims BEFORE anything spawns: the nonce-scoped staging dir
    # (plain no-clobber mkdir), the RESERVED EMPTY run destination, and the
    # O_EXCL pointer temp file — the commit later replaces the reservation
    # and the temp atomically, and cleanup owns exactly these three paths.
    mkdir -p "$evidence/runs"
    staging="$evidence/.staging-web-session-restore-$lane_nonce"
    mkdir "$staging" \
        || fail "$SCENARIO: atomic staging claim failed for nonce $lane_nonce (existing staging dir rejected)"
    staging_owned=1
    mkdir "$evidence/runs/web-session-restore-$lane_nonce" \
        || fail "$SCENARIO: atomic run-destination claim failed for nonce $lane_nonce (collision rejected)"
    run_owned=1
    pointer_tmp="$evidence/.current.json.$lane_nonce.tmp"
    node -e 'const fs = require("node:fs"); fs.closeSync(fs.openSync(process.argv[1], "wx"));' \
        "$pointer_tmp" 2>/dev/null \
        || fail "$SCENARIO: atomic pointer-temp claim failed for nonce $lane_nonce (existing temp file rejected)"
    pointer_tmp_owned=1
    # Byte-EXACT snapshot of the production pointer BEFORE this attempt, via
    # file copy (empty-file marker absent when no pointer exists yet): every
    # failure path can compare with cmp -s, never command substitution.
    lane_pointer_before_file="$root/pointer.before"
    if [ -f "$evidence/current.json" ]; then
        cp "$evidence/current.json" "$lane_pointer_before_file"
    else
        rm -f "$lane_pointer_before_file"
    fi
    # Persist a native session before listener startup so the browser exercises
    # the restored transcript path against real recorder wire shapes. The
    # fixture includes legitimate assistant/IRC content, a hidden internal
    # custom, long legacy output, and durable bash/generic toolCall/result
    # pairs whose metadata, terminal state, and bounded tails must restore.
    python3 - "$session_dir" "$root/workspace" <<'PY'
import datetime, json, pathlib, sys
session_dir = pathlib.Path(sys.argv[1])
cwd = sys.argv[2]
session_dir.mkdir(parents=True, exist_ok=True)
stamp = datetime.datetime.now(datetime.timezone.utc).isoformat().replace('+00:00', 'Z')
sid = 'web-transcript-parity'
records = [
    {'type': 'session', 'version': 3, 'id': sid, 'timestamp': stamp, 'cwd': cwd},
    # Explicit session_info name: the Web sidebar `temporary` marker must
    # never apply to this legal restored session (native + unnamed + <10KiB +
    # cwd under the OS temp root would otherwise hide it by default).
    {'type': 'session_info', 'id': 'si-1', 'parentId': None, 'timestamp': stamp,
     'name': 'web-transcript-parity'},
    {'type': 'message', 'id': 'u1', 'parentId': None, 'timestamp': stamp,
     'message': {'role': 'user', 'content': [{'type': 'text', 'text': 'transcript parity seed'}], 'timestamp': 1}},
    {'type': 'custom_message', 'id': 'hidden', 'parentId': 'u1', 'timestamp': stamp,
     'customType': 'pi.goal.active', 'content': '<system-reminder>internal parity secret</system-reminder>',
     'display': False, 'details': {}},
    {'type': 'custom_message', 'id': 'irc', 'parentId': 'hidden', 'timestamp': stamp,
     'customType': 'orchestration_message',
     'content': '<orchestration-message id="m1" from="Main">\nclean IRC parity body\nReplying to message: parent-9\n</orchestration-message>',
     'display': True, 'details': {'id': 'm1', 'from': 'Main', 'to': 'Child', 'body': 'clean IRC parity body', 'replyTo': 'parent-9'}},
    {'type': 'custom_message', 'id': 'irc-in', 'parentId': 'irc', 'timestamp': stamp,
     'customType': 'orchestration_message',
     'content': '<orchestration-message id="m2" from="Child">\n' + '\n'.join(f'irc-incoming-line-{n}' for n in range(1, 11)) + '\n</orchestration-message>',
     'display': True, 'details': {'id': 'm2', 'from': 'Child', 'to': 'Main', 'body': '\n'.join(f'irc-incoming-line-{n}' for n in range(1, 11))}},
    {'type': 'message', 'id': 'bash', 'parentId': 'irc-in', 'timestamp': stamp,
     'message': {'role': 'bashExecution', 'command': 'seq 1 30',
                 'output': '\n'.join(f'bash-parity-{n}' for n in range(1, 31)),
                 'cancelled': False, 'truncated': False, 'timestamp': 2}},
    {'type': 'message', 'id': 'bash-call', 'parentId': 'bash', 'timestamp': stamp,
     'message': {'role': 'assistant',
                 'content': [{'type': 'toolCall', 'id': 'parity-bash-card', 'name': 'bash',
                              'arguments': {'command': 'seq 1 15'}}],
                 'api': 'openai-completions', 'provider': 'user-steering', 'model': 'mock', 'stopReason': 'toolUse', 'timestamp': 3}},
    {'type': 'message', 'id': 'bash-result', 'parentId': 'bash-call', 'timestamp': stamp,
     'message': {'role': 'toolResult', 'toolCallId': 'parity-bash-card', 'toolName': 'bash',
                 'content': [{'type': 'text', 'text': '\n'.join(f'bash-card-{n}' for n in range(1, 16))}],
                 'isError': False, 'timestamp': 4}},
    {'type': 'message', 'id': 'tool-call', 'parentId': 'bash-result', 'timestamp': stamp,
     'message': {'role': 'assistant',
                 'content': [{'type': 'toolCall', 'id': 'parity-tool', 'name': 'read',
                              'arguments': {'path': 'parity.txt'}}],
                 'api': 'openai-completions', 'provider': 'user-steering', 'model': 'mock', 'stopReason': 'toolUse', 'timestamp': 5}},
    {'type': 'message', 'id': 'tool', 'parentId': 'tool-call', 'timestamp': stamp,
     'message': {'role': 'toolResult', 'toolCallId': 'parity-tool', 'toolName': 'read',
                 'content': [{'type': 'text', 'text': '\n'.join(f'tool-parity-{n}' for n in range(1, 13))}],
                 'isError': True, 'timestamp': 6}},
    {'type': 'message', 'id': 'orphan-tool', 'parentId': 'tool', 'timestamp': stamp,
     'message': {'role': 'toolResult', 'toolCallId': 'orphan-parity-tool', 'toolName': 'grep',
                 'content': [{'type': 'text', 'text': '\n'.join(f'orphan-parity-{n}' for n in range(1, 9))}],
                 'isError': False, 'timestamp': 7}},
    {'type': 'message', 'id': 'a1', 'parentId': 'orphan-tool', 'timestamp': stamp,
     'message': {'role': 'assistant', 'content': [{'type': 'text', 'text': 'system-reminder wording is legitimate assistant text'}, {'type': 'thinking', 'thinking': 'THINKPROBE_EN restored chain of thought'}],
                 'api': 'openai-completions', 'provider': 'user-steering', 'model': 'mock', 'stopReason': 'stop', 'timestamp': 8}},
    # A Task card whose recorded spawn is still queued/running: the session
    # has no orchestration runtime, so the authoritative job_list is empty
    # and the restore/reconnect reconciliation must omission-settle this
    # exact jobId instead of leaving a stale Running card (cold-cache cut).
    {'type': 'message', 'id': 'task-call', 'parentId': 'a1', 'timestamp': stamp,
     'message': {'role': 'assistant',
                 'content': [{'type': 'toolCall', 'id': 'parity-task-card', 'name': 'task',
                              'arguments': {'task': 'restore parity child work', 'agent': 'reviewer',
                                            'name': 'RestoreChild', 'context': 'restored history'}}],
                 'api': 'openai-completions', 'provider': 'user-steering', 'model': 'mock', 'stopReason': 'toolUse', 'timestamp': 9}},
    {'type': 'message', 'id': 'task-spawn', 'parentId': 'task-call', 'timestamp': stamp,
     'message': {'role': 'toolResult', 'toolCallId': 'parity-task-card', 'toolName': 'task',
                 'content': [{'type': 'text', 'text': 'spawned restore parity child'}],
                 'details': [{'index': 0, 'jobId': 'job-restore-parity', 'agentId': 'RestoreChild',
                              'agent': 'reviewer', 'status': 'running'}],
                 'isError': False, 'timestamp': 10}},
]
(session_dir / f'{stamp[:10]}T000000-{sid}.jsonl').write_text(
    ''.join(json.dumps(record, separators=(',', ':')) + '\n' for record in records),
    encoding='utf-8',
)
sid_a = 'web-transcript-a'
records_a = [
    {'type': 'session', 'version': 3, 'id': sid_a, 'timestamp': stamp, 'cwd': cwd},
    {'type': 'session_info', 'id': 'si-a', 'parentId': None, 'timestamp': stamp,
     'name': sid_a},
    {'type': 'message', 'id': 'u-a', 'parentId': None, 'timestamp': stamp,
     'message': {'role': 'user', 'content': [{'type': 'text', 'text': 'transcript A seed'}], 'timestamp': 1}},
]
(session_dir / f'{stamp[:10]}T000001-{sid_a}.jsonl').write_text(
    ''.join(json.dumps(record, separators=(',', ':')) + '\n' for record in records_a),
    encoding='utf-8',
)
PY

    # A FIXED listen port so the respawned listener reuses it (the browser
    # reconnects to the same URL without a page reload).
    listen_port="$({ python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1]); s.close()' 2>/dev/null; } | tr -d '[:space:]')"
    [ -n "$listen_port" ] || fail "could not allocate a fixed listen port"

    port="$(MOCK_SCENARIO="$MOCK_SCENARIO" web_start_mock "$root" "$evidence")"
    web_spawn_rpi "$root" "$evidence" "$port" "$listen_port"
    url="$(web_wait_for_listener "$evidence")"
    log "$SCENARIO: listener at $url"

    rm -f "$root/playwright/kill-server.marker" "$root/playwright/server-up.marker"
    mkdir -p "$root/playwright"

    web_run_playwright "$url" "$evidence" "$root/playwright" "$SCRIPT_DIR/session_restore_test.mjs" \
        "RPI_REPLY=sessions-reply:" \
        "RPI_PARITY_SESSION=web-transcript-parity" \
        "RPI_BASE_SESSION=web-transcript-a" \
        "RPI_LANE_NONCE=$lane_nonce" \
        "RPI_STAGING=$staging" \
        >"$evidence/playwright.out" 2>&1 &
    pw_pid=$!
    # Register the playwright child so the EXIT/HUP/INT/TERM trap reaps it
    # if the lane is killed externally (e.g. by a wrapping timeout). pw_pid
    # is a function-local otherwise unreaped by cleanup_e2e; a stuck child
    # (here: the test's setInterval kept Node alive) would leak on interrupt.
    register_pid "$pw_pid"

    # Wait for the test's restart request; respawn the listener on the SAME port.
    local deadline=$((SECONDS + 180))
    while [ ! -f "$root/playwright/kill-server.marker" ] && [ "$SECONDS" -lt "$deadline" ]; do
        if ! kill -0 "$pw_pid" 2>/dev/null; then
            fail "$SCENARIO: playwright exited before requesting the restart (see $evidence/playwright.out)"
        fi
        sleep 0.3
    done
    if [ ! -f "$root/playwright/kill-server.marker" ]; then
        fail "$SCENARIO: test never wrote the kill-server marker"
    fi

    log "$SCENARIO: restarting the listener through the SIGTERM restart authority on port $listen_port"
    web_kill_rpi
    # Respawn WITHOUT --continue: a Web-only restart starts a fresh primary;
    # the recorded session is reopened by switching to it from the sidebar.
    web_spawn_rpi "$root" "$evidence" "$port" "$listen_port" "2"
    if ! web_wait_for_listener "$evidence" "2" >/dev/null 2>&1; then
        fail "$SCENARIO: respawned listener never came back (see $evidence/rpi2.stderr)"
    fi
    printf 'up\n' >"$root/playwright/server-up.marker"

    wait "$pw_pid" || pw_status=$?
    if [ "$pw_status" -eq 0 ]; then
        # Playwright STAGED ONLY — nothing is published yet. The pointer can
        # never name a run before the HTTP sanity and listener-close gates
        # below have passed.
        grep -Fqx "web-session-restore: STAGED $staging" "$evidence/playwright.out" \
            || fail "$SCENARIO: playwright exited 0 without its STAGED marker — see $evidence/playwright.out"
        printf 'playwright: session parity lane staged\n' | tee "$root/playwright-summary.txt"
    else
        fail "$SCENARIO: playwright lane failed (exit $pw_status) — see $evidence/playwright.out"
    fi

    # Final gates BEFORE the sole publication commit. Gate 1: the listener
    # must still serve / (web_sanity_http). Gate 2: SIGTERM must ACTUALLY
    # stop it — Web-only listen never reads stdin, so REPL EOF is not a
    # shutdown authority.
    web_sanity_http "$url"
    log "$SCENARIO: stopping the listener through SIGTERM and verifying exit"
    listener_pid="${RPI_PID:-}"
    web_kill_rpi
    if [ -n "$listener_pid" ] && kill -0 "$listener_pid" 2>/dev/null; then
        fail "$SCENARIO: listener did not exit after SIGTERM — publication commit withheld"
    fi

    # Gate 3 — THE sole publication commit: exact staged-set/ledger
    # validation, ONE stage-dir -> immutable run-dir rename, ONE
    # atomically-renamed current.json pointer. HUP/INT/TERM are BLOCKED
    # across the commit AND the lane_committed transition: a signal can
    # never split the two-step publication. After the traps are restored
    # there is NO fallible action left except the PASS line and exit 0.
    local commit_ok=0
    trap '' HUP INT TERM
    commit_sr_run "$staging" "$lane_nonce" "$pointer_tmp" && commit_ok=1
    if [ "$commit_ok" = 1 ]; then
        lane_committed=1
    fi
    trap 'on_sr_lane_signal HUP' HUP
    trap 'on_sr_lane_signal INT' INT
    trap 'on_sr_lane_signal TERM' TERM
    [ "$commit_ok" = 1 ] \
        || fail "$SCENARIO: publication commit failed — staging/run/temp removed, prior pointer preserved"
    log "$SCENARIO: published $evidence/runs/web-session-restore-$lane_nonce via $evidence/current.json"
    printf '%s passed\nevidence=%s\n' "$SCENARIO" "$EVIDENCE_ROOT/$SCENARIO"
}

main "$@"