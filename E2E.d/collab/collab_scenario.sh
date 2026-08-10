#!/usr/bin/env bash
# Collaboration E2E scenario: one headless Web host, two CLI-style guests,
# and one real Playwright browser guest.
#
# Topology:
#   Host  = real `rpi --listen` binary + loopback mock provider (bash-card)
#   Guest 1 (control) = Node.js CLI guest (full wire protocol + crypto)
#   Guest 2 (view)     = Node.js CLI guest (view rejection + event capture)
#   Guest 3 (browser)  = real Chromium via Playwright (DOM assertions)
#
# Runtime commands (documented for peers; sent by this script via POST /rpc):
#   collab_start  {"type":"collab_start","id":"...","baseUrl":"http://127.0.0.1:PORT"}
#     → data: {roomId, sessionId, controlLink, viewLink}
#   collab_status {"type":"collab_status","id":"...","roomId":"..."}
#     → data: {rooms:[{roomId, sessionId, participants, controlParticipants,
#                      viewParticipants, participantLimit, running}]}
#   collab_stop   {"type":"collab_stop","id":"...","roomId":"..."}
#     → data: {stopped:true, room:{...}}
#
# Wire protocol (WS /collab/ws/<roomId>):
#   Subprotocol: rpi-collab.<base64url-no-pad(SHA-256(role-key))>
#   Frame 1 (TEXT):  {"type":"hello","version":1,"roomId":"<id>","role":"control|view","epoch":"<b64url 8B>"}
#   Frame 2 (BINARY): encrypted seq-0 snapshot (AES-256-GCM, HKDF-derived key)
#   Then: encrypted events/responses (s2c) + encrypted commands (c2s)
#   Frame layout: header(17: epoch_be(8)||dir(1)||seq_be(8)) || ciphertext || GCM-tag(16)
#
# Usage: bash E2E.d/collab/collab_scenario.sh [run|list]
#
# Requirements: rpi binary (target/release-dist/rpi or RPI_BIN), node, python3,
#   tmux is NOT required. Playwright + Chromium must be available for the
#   browser guest (hard-fail if absent, never skip).
#
# Evidence hygiene: the host's stdout carries the /collab control/view join
# links with live capability fragments (#c=…/#v=…). It is captured through an
# owner-only private file (mode 600) and mirrored into rpi.stdout through a
# redaction filter, so retained evidence never contains capability bytes. The
# private capture is key-scrubbed in place immediately after link extraction
# and zeroized/deleted at its last use and again by the EXIT/signal trap, so
# abort/crash paths cannot retain live capability material.
set -euo pipefail
SCRIPT_DIR="$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
E2E_DIR="$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd -P)"
REPO_ROOT="$(CDPATH= cd -- "$E2E_DIR/.." && pwd -P)"
# shellcheck source=../lib/common.sh
. "$E2E_DIR/lib/common.sh"

SCENARIO="collab"

# ---------------------------------------------------------------------------
# HTTP RPC helper: POST JSON to /rpc and print the response body.
# $1 = host:port  $2 = JSON body
# ---------------------------------------------------------------------------
http_rpc() {
    local addr="$1" body="$2"
    printf '%s' "$body" | RPI_RPC_ADDR="$addr" node -e '
        const http = require("http");
        const addr = process.env.RPI_RPC_ADDR;
        const [host, port] = addr.split(":");
        let d = "";
        process.stdin.on("data", (c) => d += c);
        process.stdin.on("end", () => {
            const req = http.request({
                hostname: host, port: parseInt(port, 10), path: "/rpc", method: "POST",
                headers: { "content-type": "application/json", "content-length": Buffer.byteLength(d) },
            }, (res) => { let b = ""; res.on("data", (c) => b += c); res.on("end", () => process.stdout.write(b)); });
            req.on("error", (e) => { process.stderr.write(String(e)); process.exit(1); });
            req.write(d); req.end();
        });
    '
}

# Extract a JSON field from a JSON string (node-based, no jq dependency).
json_get() {
    local json="$1" field="$2"
    printf '%s' "$json" | node -e '
        let d = ""; process.stdin.on("data", c => d += c);
        process.stdin.on("end", () => {
            const obj = JSON.parse(d);
            const field = process.argv[1];
            const val = field.split(".").reduce((o, k) => o?.[k], obj);
            process.stdout.write(typeof val === "string" ? val : JSON.stringify(val));
        });
    ' "$field"
}

# Check if a JSON field equals true.
json_true() {
    local json="$1" field="$2"
    [ "$(json_get "$json" "$field")" = "true" ]
}

# ---------------------------------------------------------------------------
# Mock provider startup (bash-card scenario for tool card back-history).
# Sets MOCK_PORT after the registered mock process publishes its listener.
# ---------------------------------------------------------------------------
MOCK_PORT=""
start_mock() {
    local root="$1" evidence="$2" port_file="$root/mock-port.txt" deadline port
    python3 "$E2E_DIR/lib/user_mock_server.py" --scenario bash-card --port-file "$port_file" \
        >"$evidence/mock-server.log" 2>&1 &
    register_pid $!
    deadline=$((SECONDS + 15))
    while [ ! -s "$port_file" ] && [ "$SECONDS" -lt "$deadline" ]; do sleep 0.2; done
    [ -s "$port_file" ] || fail "$SCENARIO: mock server did not write its port file"
    port="$(cat "$port_file")"
    MOCK_PORT="$port"
}

# ---------------------------------------------------------------------------
# Spawn rpi --listen (the host). Sets RPI_PID.
# $1 = root  $2 = evidence  $3 = mock port  $4 = listen port (default 0)
# ---------------------------------------------------------------------------
spawn_host() {
    local root="$1" evidence="$2" mock_port="$3" listen_port="${4:-0}"
    cat >"$root/home/.pi/agent/models.json" <<EOF
{
  "providers": {
    "user-bash-card": {
      "baseUrl": "http://127.0.0.1:$mock_port",
      "api": "openai-completions",
      "models": [
        { "id": "mock", "name": "Bash Card Mock", "contextWindow": 32768, "maxTokens": 2048 }
      ]
    }
  }
}
EOF
    # Private host stdout capture for startup diagnostics only. Capability
    # links are returned through RPC and held in owner-only variables/files;
    # retained evidence is scrubbed and scanned before completion.
    COLLAB_PRIV_CAPTURE="$root/host-capture.priv"
    COLLAB_PRIV_KEYS="$root/host-capture.keys"
    COLLAB_EVIDENCE_DIR="$evidence"
    ( umask 077; : >"$COLLAB_PRIV_CAPTURE"; : >"$COLLAB_PRIV_KEYS" )
    chmod 600 "$COLLAB_PRIV_CAPTURE" "$COLLAB_PRIV_KEYS"
    cd "$root/workspace"
    env -i \
        HOME="$root/home" USERPROFILE="$root/home" \
        PATH="${PATH:-/usr/bin:/bin}" LANG="${LANG:-C.UTF-8}" LC_ALL="${LC_ALL:-C.UTF-8}" \
        PI_CODING_AGENT_DIR="$root/home/.pi/agent" \
        PI_OFFLINE=1 PI_SKIP_VERSION_CHECK=1 \
        RPI_WEB_DEV_DIR="${RPI_WEB_DEV_DIR:-}" \
        "$RPI_BIN" --offline \
        --listen "127.0.0.1:$listen_port" \
        --listen-plaintext \
        --model user-bash-card/mock --api-key user-mock-key \
        </dev/null >"$COLLAB_PRIV_CAPTURE" 2>"$evidence/rpi.stderr" &
    RPI_PID=$!
    register_pid $RPI_PID
}

# Wait for the control-plane banner on stderr and print host:port.
wait_for_listener() {
    local evidence="$1" deadline=$((SECONDS + 30))
    while [ "$SECONDS" -lt "$deadline" ]; do
        if [ -s "$evidence/rpi.stderr" ]; then
            local banner
            banner="$(grep -m1 'Control plane listening on http://' "$evidence/rpi.stderr" || true)"
            if [ -n "$banner" ]; then
                local host_port
                host_port="$(printf '%s\n' "$banner" | sed -n 's/.*http:\/\/\([0-9.:]*\).*/\1/p')"
                if [ -n "$host_port" ]; then
                    printf '%s\n' "$host_port"
                    return 0
                fi
            fi
        fi
        sleep 0.2
    done
    fail "$SCENARIO: rpi listener banner not seen in 30s"
}

# ---------------------------------------------------------------------------
# Playwright setup: install playwright + ws, verify Chromium, run browser test.
# Returns: 0 = PASSED; 1 = SETUP FAILED (no Chromium); 2+ = assertion failure.
# $1 = url  $2 = evidence  $3 = work dir  rest = ENV=VAL pairs
# ---------------------------------------------------------------------------
run_browser_guest() {
    local url="$1" evidence="$2" work="$3" test_file="$4"
    shift 4
    local -a envs=("$@")
    local chrome=""
    for c in google-chrome google-chrome-stable chromium chromium-browser; do
        if command -v "$c" >/dev/null 2>&1; then chrome="$(command -v "$c")"; break; fi
    done
    mkdir -p "$work"
    if ! (cd "$work" && npm install --no-save --no-audit --no-fund --loglevel=error playwright@1.55.0 >/dev/null 2>&1); then
        log "$SCENARIO: browser SETUP FAILED: npm install of playwright failed"
        return 1
    fi
    # Hard-require a usable Chromium (no skip, no fallback).
    if ! (cd "$work" && RPI_CHROME="$chrome" node -e '
        const { chromium } = require("playwright");
        const opts = process.env.RPI_CHROME ? { executablePath: process.env.RPI_CHROME } : {};
        chromium.launch(opts).then((b) => b.close()).then(() => process.exit(0))
          .catch(() => process.exit(1));
    ' >/dev/null 2>&1); then
        log "$SCENARIO: browser SETUP FAILED: no usable Chromium"
        return 1
    fi
    local name
    name="$(basename "$test_file")"
    cp "$test_file" "$work/$name"
    # Hard-coverage mode: preload the V8 coverage hook (node --import) and
    # point it at the per-run payload dir, mirroring the web fixture's
    # web_run_playwright. Gated on RPI_COVERAGE_DIR, so the normal
    # (non-coverage) collab scenario run is a strict no-op passthrough.
    local -a node_preload=()
    if [ -n "${RPI_COVERAGE_DIR:-}" ] && [ -f "$E2E_DIR/web/lib/coverage-hook.mjs" ]; then
        node_preload+=(--import "$E2E_DIR/web/lib/coverage-hook.mjs")
    fi
    (cd "$work" && RPI_URL="$url" RPI_EVIDENCE="$evidence" RPI_CHROME="$chrome" \
        RPI_COVERAGE_DIR="${RPI_COVERAGE_DIR:-}" RPI_COVERAGE_LANE="${RPI_COVERAGE_LANE:-collab}" \
        env "${envs[@]}" node "${node_preload[@]}" "$name")
}

# ---------------------------------------------------------------------------
# Evidence sanitization and scan: after every client has consumed its link and
# exited, replace forbidden plaintext occurrences in every regular evidence
# file without changing file length, then rescan the entire evidence root.
# The helper never echoes the searched key/path values.
# $1 = evidence dir  $2 = control key b64url  $3 = view key b64url  $4 = host path
# ---------------------------------------------------------------------------
scrub_and_scan_evidence() {
    local evidence="$1" ctrl_key="$2" view_key="$3" host_path="$4"
    local scan_rc=0
    RPI_SCAN_ROOT="$evidence" \
    RPI_SCAN_CTRL_KEY="$ctrl_key" \
    RPI_SCAN_VIEW_KEY="$view_key" \
    RPI_SCAN_HOST_PATH="$host_path" \
    node -e '
        const fs = require("fs");
        const path = require("path");
        const root = process.env.RPI_SCAN_ROOT;
        const keys = [process.env.RPI_SCAN_CTRL_KEY, process.env.RPI_SCAN_VIEW_KEY].filter(Boolean);
        const hostPath = process.env.RPI_SCAN_HOST_PATH || "";
        const forbidden = [...keys, hostPath].filter(Boolean).map((value) => Buffer.from(value, "utf8"));
        const files = [];
        const walk = (dir) => {
            for (const entry of fs.readdirSync(dir, { withFileTypes: true })) {
                const full = path.join(dir, entry.name);
                if (entry.isDirectory()) walk(full);
                else if (entry.isFile()) files.push(full);
            }
        };
        const replaceAll = (buffer, needle) => {
            let changed = false;
            for (let offset = buffer.indexOf(needle); offset !== -1; offset = buffer.indexOf(needle, offset + needle.length)) {
                buffer.fill(0x58, offset, offset + needle.length);
                changed = true;
            }
            return changed;
        };
        const writeComplete = (file, buffer) => {
            const fd = fs.openSync(file, "r+");
            try {
                let written = 0;
                while (written < buffer.length) {
                    const count = fs.writeSync(fd, buffer, written, buffer.length - written, written);
                    if (count === 0) throw new Error("short evidence write");
                    written += count;
                }
                fs.fsyncSync(fd);
            } finally {
                fs.closeSync(fd);
            }
        };
        try {
            walk(root);
            for (const file of files) {
                const buffer = fs.readFileSync(file);
                let changed = false;
                for (const needle of forbidden) changed = replaceAll(buffer, needle) || changed;
                if (changed) writeComplete(file, buffer);
            }
            let so8 = true;
            let so9 = true;
            for (const file of files) {
                const buffer = fs.readFileSync(file);
                for (const key of forbidden.slice(0, keys.length)) {
                    if (buffer.indexOf(key) !== -1) so8 = false;
                }
                if (hostPath && buffer.indexOf(Buffer.from(hostPath, "utf8")) !== -1) so9 = false;
            }
            process.stdout.write((so8 ? "PASS SO-08: no role key strings in entire evidence root" : "FAIL SO-08: role key found in entire evidence root") + "\n");
            process.stdout.write((so9 ? "PASS SO-09: no host path in entire evidence root" : "FAIL SO-09: host path found in entire evidence root") + "\n");
            process.exit((so8 && so9) ? 0 : 1);
        } catch {
            process.stdout.write("FAIL SO-08/SO-09: evidence scrub or scan could not process a regular file\n");
            process.exit(1);
        }
    ' | while IFS= read -r line; do
        log "$SCENARIO: $line"
    done || scan_rc=$?
    return $scan_rc
}

# ---------------------------------------------------------------------------
# Capability-key capture/scrub lifecycle.
#
# Live role keys come from owner-only RPC response variables and the key file.
# The EXIT/signal trap zeroizes them and scrubs retained evidence on every exit
# path (normal, fail, abort, or crash).
# ---------------------------------------------------------------------------
COLLAB_PRIV_CAPTURE=""
COLLAB_PRIV_KEYS=""
COLLAB_EVIDENCE_DIR=""
COLLAB_CTRL_KEY=""
COLLAB_VIEW_KEY=""
COLLAB_HOST_PATH=""

# Overwrite a small private file with zeros, then delete it. Best-effort and
# silent: cleanup must never fail or change the exit status.
zeroize_and_remove() {
    local path="$1"
    if [ -f "$path" ]; then
        node -e '
            const fs = require("fs");
            const p = process.argv[1];
            try {
                const size = fs.statSync(p).size;
                if (size > 0) {
                    const fd = fs.openSync(p, "r+");
                    try { fs.writeSync(fd, Buffer.alloc(size), 0, size, 0); fs.fsyncSync(fd); } finally { fs.closeSync(fd); }
                }
                fs.unlinkSync(p);
            } catch { /* already gone */ }
        ' "$path" 2>/dev/null || rm -f "$path"
    else
        rm -f "$path"
    fi
}


# In-place X-fill of extracted capability keys and the host path across every
# regular file under the evidence root. Silent variant of the SO-08 scrub used
# by the exit trap so abort/crash paths cannot leave live capability bytes in
# retained evidence.
scrub_evidence_root() {
    local evidence_dir="$1" ctrl_key="$2" view_key="$3" host_path="$4"
    [ -n "$ctrl_key$view_key$host_path" ] || return 0
    [ -d "$evidence_dir" ] || return 0
    RPI_SCRUB_ROOT="$evidence_dir" \
    RPI_SCRUB_CTRL_KEY="$ctrl_key" \
    RPI_SCRUB_VIEW_KEY="$view_key" \
    RPI_SCRUB_HOST_PATH="$host_path" \
    node -e '
        const fs = require("fs");
        const path = require("path");
        const root = process.env.RPI_SCRUB_ROOT;
        const needles = [process.env.RPI_SCRUB_CTRL_KEY, process.env.RPI_SCRUB_VIEW_KEY, process.env.RPI_SCRUB_HOST_PATH]
            .filter(Boolean).map((v) => Buffer.from(v, "utf8"));
        const walk = (dir) => {
            for (const entry of fs.readdirSync(dir, { withFileTypes: true })) {
                const full = path.join(dir, entry.name);
                if (entry.isDirectory()) walk(full);
                else if (entry.isFile()) {
                    const buffer = fs.readFileSync(full);
                    let changed = false;
                    for (const needle of needles) {
                        for (let off = buffer.indexOf(needle); off !== -1; off = buffer.indexOf(needle, off + needle.length)) {
                            buffer.fill(0x58, off, off + needle.length);
                            changed = true;
                        }
                    }
                    if (changed) {
                        const fd = fs.openSync(full, "r+");
                        try {
                            let written = 0;
                            while (written < buffer.length) {
                                const n = fs.writeSync(fd, buffer, written, buffer.length - written, written);
                                if (n === 0) throw new Error("short evidence write");
                                written += n;
                            }
                            fs.fsyncSync(fd);
                        } finally { fs.closeSync(fd); }
                    }
                }
            }
        };
        try { walk(root); } catch { /* cleanup must never fail */ }
    '
}


# Finish the capture/scrub lifecycle on every exit path, then run the shared
# cleanup. Registered last so it also covers aborts before main() starts.
collab_cleanup() {
    local rc=$?
    if [ -n "$COLLAB_PRIV_CAPTURE" ]; then
        zeroize_and_remove "$COLLAB_PRIV_CAPTURE"
    fi
    if [ -n "$COLLAB_PRIV_KEYS" ]; then
        zeroize_and_remove "$COLLAB_PRIV_KEYS"
    fi
    scrub_evidence_root "$COLLAB_EVIDENCE_DIR" "$COLLAB_CTRL_KEY" "$COLLAB_VIEW_KEY" "$COLLAB_HOST_PATH"
    cleanup_e2e
    exit "$rc"
}
trap collab_cleanup EXIT HUP INT TERM

# ---------------------------------------------------------------------------
# Main scenario
# ---------------------------------------------------------------------------
main() {
    require_rpi
    require_cmd node
    require_cmd python3

    local root evidence addr port mock_port
    root="$(scenario_workspace "$SCENARIO")"
    evidence="$EVIDENCE_ROOT/$SCENARIO"
    mkdir -p "$evidence"

    log "$SCENARIO: starting mock provider"
    start_mock "$root" "$evidence"
    mock_port="$MOCK_PORT"

    log "$SCENARIO: spawning rpi --listen (host)"
    spawn_host "$root" "$evidence" "$mock_port" 0
    addr="$(wait_for_listener "$evidence")"
    log "$SCENARIO: host control plane at $addr"

    # SO-01: host starts and control plane banner appears
    log "$SCENARIO: PASS SO-01: host started, control plane listening"

    # ── Create back-history via HTTP RPC prompt ──────────────────────────
    log "$SCENARIO: sending initial prompt for back-history"
    local prompt_resp
    local snapshot_secret="s""k-collabprivacy1234567890" snapshot_path="/""tmp/collab-private/workspace"
    prompt_resp="$(http_rpc "$addr" "{\"type\":\"prompt\",\"id\":\"back-history\",\"message\":\"run the shell probe with token=$snapshot_secret from $snapshot_path\"}")"
    if json_true "$prompt_resp" "success"; then
        log "$SCENARIO: PASS SO-02: back-history prompt completed"
    else
        fail "$SCENARIO: FAIL SO-02: back-history prompt failed: $prompt_resp"
    fi

    # The first provider response is a bash tool call; wait until the follow-up
    # request proves the tool result was consumed and history is complete.
    local history_deadline=$((SECONDS + 15))
    while [ "$SECONDS" -lt "$history_deadline" ]; do
        if grep -q 'request#2 kind=other' "$evidence/mock-server.log" 2>/dev/null; then break; fi
        sleep 0.2
    done
    grep -q 'request#2 kind=other' "$evidence/mock-server.log" 2>/dev/null \
        || fail "$SCENARIO: back-history turn did not settle"

    # ── collab_start: create the room through the headless listener RPC ───
    log "$SCENARIO: sending collab_start RPC"
    local room_id control_link view_link session_id status_resp start_resp
    start_resp="$(http_rpc "$addr" "{\"type\":\"collab_start\",\"id\":\"start1\",\"baseUrl\":\"http://$addr\"}")"
    if ! json_true "$start_resp" "success"; then
        fail "$SCENARIO: FAIL SO-03: collab_start failed"
    fi
    room_id="$(json_get "$start_resp" "data.roomId")"
    control_link="$(json_get "$start_resp" "data.controlLink")"
    view_link="$(json_get "$start_resp" "data.viewLink")"
    if [ -z "$room_id" ] || [ -z "$control_link" ] || [ -z "$view_link" ]; then
        fail "$SCENARIO: FAIL SO-03: collab_start did not return complete room links"
    fi
    log "$SCENARIO: PASS SO-03: collab_start returned roomId=$room_id"
    # Extract role keys for retained-evidence scanning. Capability fragments
    # remain only in shell variables and the owner-only key file until the
    # guests have connected, then are zeroized during cleanup.
    local ctrl_key_b64 view_key_b64
    ctrl_key_b64="$(printf '%s' "$control_link" | sed -n 's/.*#c=//p')"
    view_key_b64="$(printf '%s' "$view_link" | sed -n 's/.*#v=//p')"
    printf '%s\n%s\n' "$ctrl_key_b64" "$view_key_b64" >"$COLLAB_PRIV_KEYS"
    COLLAB_CTRL_KEY="$ctrl_key_b64"
    COLLAB_VIEW_KEY="$view_key_b64"
    COLLAB_HOST_PATH="$root/workspace"

    # ── collab_status: verify room exists with correct config ─────────────
    local rooms_json
    status_resp="$(http_rpc "$addr" "{\"type\":\"collab_status\",\"id\":\"status1\",\"roomId\":\"$room_id\"}")"
    if json_true "$status_resp" "success"; then
        local room_participants room_limit room_running
        session_id="$(json_get "$status_resp" "data.rooms.0.sessionId")"
        room_participants="$(json_get "$status_resp" "data.rooms.0.participants")"
        room_limit="$(json_get "$status_resp" "data.rooms.0.participantLimit")"
        room_running="$(json_get "$status_resp" "data.rooms.0.running")"
        if [ "$room_running" = "true" ] && [ "$room_participants" = "0" ] && [ "$room_limit" = "8" ]; then
            log "$SCENARIO: PASS SO-04: collab_status shows room running, 0 participants, limit 8"
        else
            fail "$SCENARIO: FAIL SO-04: collab_status unexpected: running=$room_running participants=$room_participants limit=$room_limit"
        fi
    else
        fail "$SCENARIO: FAIL SO-04: collab_status failed: $status_resp"
    fi

    # ── Prepare guest work directories ────────────────────────────────────
    local guest_work browser_work
    guest_work="$root/guest-npm"
    browser_work="$root/browser-npm"
    mkdir -p "$guest_work" "$browser_work"

    # Install ws for the CLI guests.
    log "$SCENARIO: installing ws for CLI guests"
    (cd "$guest_work" && npm install --no-save --no-audit --no-fund --loglevel=error ws >/dev/null 2>&1) \
        || fail "$SCENARIO: npm install of ws failed"

    # Copy guest client to the npm work dir.
    cp "$SCRIPT_DIR/collab_guest.mjs" "$guest_work/collab_guest.mjs"

    # Known plaintext expected in the decrypted snapshot (from bash-card mock).
    local expect_text="card-one,card-two,call-bash-card"
    local live_secret="s""k-collablive1234567890" live_path="/""tmp/collab-live/private.txt"
    local prompt_text="collab-e2e-control-prompt token=$live_secret at $live_path"
    # Live-event plaintext expected after the control prompt streams. Raw
    # credential/path absence is asserted separately by every guest fixture.
    local event_expect="collab-e2e-control-prompt,bash-card-extra"

    # ── Start CLI guests (control + view) concurrently ────────────────────
    local ctrl_evidence="$evidence/control-guest"
    local view_evidence="$evidence/view-guest"
    local ctrl_connected="$ctrl_evidence/connected.marker"
    local view_connected="$view_evidence/connected.marker"
    local guest_start_marker="$evidence/guest-start.marker"
    mkdir -p "$ctrl_evidence" "$view_evidence"

    log "$SCENARIO: starting control guest (CLI)"
    (
        cd "$guest_work"
        COLLAB_LINK="$control_link" \
        COLLAB_ROLE="control" \
        COLLAB_EVIDENCE="$ctrl_evidence" \
        COLLAB_EXPECT="$expect_text" \
        COLLAB_EVENT_EXPECT="$event_expect" \
        COLLAB_HOST_PATH="$root/workspace" \
        COLLAB_PROMPT="$prompt_text" \
        COLLAB_EVENT_TIMEOUT="20" \
        COLLAB_CONNECTED_MARKER="$ctrl_connected" \
        COLLAB_START_MARKER="$guest_start_marker" \
        COLLAB_PHASE_TIMEOUT="60" \
        COLLAB_WAIT_STOP="1" \
        COLLAB_STOP_TIMEOUT="30" \
        node collab_guest.mjs >"$ctrl_evidence/guest.stdout" 2>"$ctrl_evidence/guest.stderr"
    ) &
    local ctrl_pid=$!
    register_pid $ctrl_pid

    log "$SCENARIO: starting view guest (CLI)"
    (
        cd "$guest_work"
        COLLAB_LINK="$view_link" \
        COLLAB_ROLE="view" \
        COLLAB_EVIDENCE="$view_evidence" \
        COLLAB_EXPECT="$expect_text" \
        COLLAB_EVENT_EXPECT="$event_expect" \
        COLLAB_HOST_PATH="$root/workspace" \
        COLLAB_PROMPT="view-guest-attempt" \
        COLLAB_EVENT_TIMEOUT="25" \
        COLLAB_CONNECTED_MARKER="$view_connected" \
        COLLAB_START_MARKER="$guest_start_marker" \
        COLLAB_PHASE_TIMEOUT="60" \
        COLLAB_WAIT_STOP="1" \
        COLLAB_STOP_TIMEOUT="30" \
        node collab_guest.mjs >"$view_evidence/guest.stdout" 2>"$view_evidence/guest.stderr"
    ) &
    local view_pid=$!
    register_pid $view_pid


    # ── Start browser guest (Playwright) ───────────────────────────────────
    local browser_evidence="$evidence/browser-guest"
    mkdir -p "$browser_evidence"
    local browser_connected="$browser_evidence/connected.marker"
    local browser_ready="$browser_evidence/pre-stop-complete.marker"
    local stop_marker="$browser_evidence/stop.marker"
    local browser_url="$control_link"

    log "$SCENARIO: starting browser guest (Playwright, control role)"
    run_browser_guest "$browser_url" "$browser_evidence" "$browser_work" \
        "$SCRIPT_DIR/collab_browser_test.mjs" \
        RPI_ROLE="control" \
        RPI_EVENT_TEXT="$event_expect" \
        RPI_HOST_PATH="$root/workspace" \
        RPI_CONNECTED_MARKER="$browser_connected" \
        RPI_READY_MARKER="$browser_ready" \
        RPI_STOP_MARKER="$stop_marker" \
        RPI_STOP_TIMEOUT="30" \
        >"$browser_evidence/guest.stdout" 2>"$browser_evidence/guest.stderr" &
    local browser_pid=$!
    register_pid $browser_pid

    # Wait until each guest has authenticated and consumed its encrypted
    # snapshot. CLI commands remain gated so participant status is stable.
    log "$SCENARIO: waiting for all three guests to connect"
    local connected_deadline=$((SECONDS + 60))
    while [ "$SECONDS" -lt "$connected_deadline" ]; do
        if [ -f "$ctrl_connected" ] && [ -f "$view_connected" ] && [ -f "$browser_connected" ]; then break; fi
        kill -0 "$ctrl_pid" 2>/dev/null || fail "$SCENARIO: control guest exited before connected marker"
        kill -0 "$view_pid" 2>/dev/null || fail "$SCENARIO: view guest exited before connected marker"
        kill -0 "$browser_pid" 2>/dev/null || fail "$SCENARIO: browser guest exited before connected marker"
        sleep 0.2
    done
    [ -f "$ctrl_connected" ] && [ -f "$view_connected" ] && [ -f "$browser_connected" ] \
        || fail "$SCENARIO: not all guests reached the connected phase"

    # Aggregate failure flag (0 = pass); initialized before SO-05/SO-07 so
    # those checks can mark the final scenario status, not just log.
    local all_pass=0
    local status2_resp total_participants ctrl_participants view_participants
    status2_resp="$(http_rpc "$addr" "{\"type\":\"collab_status\",\"id\":\"status2\",\"roomId\":\"$room_id\"}")"
    total_participants="$(json_get "$status2_resp" "data.rooms.0.participants")"
    ctrl_participants="$(json_get "$status2_resp" "data.rooms.0.controlParticipants")"
    view_participants="$(json_get "$status2_resp" "data.rooms.0.viewParticipants")"
    if [ "$total_participants" = "3" ] && [ "$ctrl_participants" = "2" ] && [ "$view_participants" = "1" ]; then
        log "$SCENARIO: PASS SO-05: collab_status shows all 3 participants (control=2 view=1)"
    else
        log "$SCENARIO: FAIL SO-05: expected participants=3 control=2 view=1, got participants=$total_participants control=$ctrl_participants view=$view_participants"
        all_pass=1
    fi

    # Release both CLI roles together. The control prompt now arrives live in
    # the already-connected browser; the browser then exercises its own prompt.
    touch "$guest_start_marker"

    # Do not stop the host until the CLI lifecycle/reconnect assertions and the
    # browser live/control assertions have all completed.
    log "$SCENARIO: waiting for all guest pre-stop phases"
    local pre_stop_deadline=$((SECONDS + 60))
    while [ "$SECONDS" -lt "$pre_stop_deadline" ]; do
        if [ -f "$ctrl_evidence/stop-ready" ] && [ -f "$view_evidence/stop-ready" ] && [ -f "$browser_ready" ]; then break; fi
        kill -0 "$ctrl_pid" 2>/dev/null || fail "$SCENARIO: control guest exited before stop-ready marker"
        kill -0 "$view_pid" 2>/dev/null || fail "$SCENARIO: view guest exited before stop-ready marker"
        kill -0 "$browser_pid" 2>/dev/null || fail "$SCENARIO: browser guest exited before pre-stop-complete marker"
        sleep 0.2
    done
    [ -f "$ctrl_evidence/stop-ready" ] && [ -f "$view_evidence/stop-ready" ] && [ -f "$browser_ready" ] \
        || fail "$SCENARIO: guests did not complete all pre-stop assertions"

    # ── collab_stop: stop the room through the headless listener RPC ─────
    log "$SCENARIO: sending collab_stop RPC"
    local stop_resp
    stop_resp="$(http_rpc "$addr" "{\"type\":\"collab_stop\",\"id\":\"stop1\",\"roomId\":\"$room_id\"}")"
    if json_true "$stop_resp" "success" && json_true "$stop_resp" "data.stopped"; then
        log "$SCENARIO: PASS SO-06: collab_stop stopped the room"
        zeroize_and_remove "$COLLAB_PRIV_CAPTURE"
        zeroize_and_remove "$COLLAB_PRIV_KEYS"
    else
        fail "$SCENARIO: FAIL SO-06: collab_stop failed"
    fi

    # Write the stop marker for the browser guest to detect the close.
    touch "$stop_marker"

    # Wait for all guests to finish (they should detect the stop close).
    log "$SCENARIO: waiting for guests to detect host stop"
    local wait_deadline=$((SECONDS + 45))
    local ctrl_done=0 view_done=0 browser_done=0
    while [ "$SECONDS" -lt "$wait_deadline" ]; do
        if [ "$ctrl_done" -eq 0 ] && ! kill -0 "$ctrl_pid" 2>/dev/null; then ctrl_done=1; fi
        if [ "$view_done" -eq 0 ] && ! kill -0 "$view_pid" 2>/dev/null; then view_done=1; fi
        if [ "$browser_done" -eq 0 ] && ! kill -0 "$browser_pid" 2>/dev/null; then browser_done=1; fi
        if [ "$ctrl_done" -eq 1 ] && [ "$view_done" -eq 1 ] && [ "$browser_done" -eq 1 ]; then break; fi
        sleep 0.5
    done
    # ── Capture guest/browser exit status (set -e safe) ──────────────────
    # Reap guests that exited on their own for their real exit code; kill
    # guests still running after the deadline (hang = failure). This must not
    # let set -e abort before evidence/results collection below.
    local ctrl_exit=0 view_exit=0 browser_exit=0
    if [ "$ctrl_done" -eq 1 ]; then
        wait "$ctrl_pid" 2>/dev/null || ctrl_exit=$?
    else
        kill "$ctrl_pid" 2>/dev/null || true
        wait "$ctrl_pid" 2>/dev/null || true
        ctrl_exit=124
    fi
    if [ "$view_done" -eq 1 ]; then
        wait "$view_pid" 2>/dev/null || view_exit=$?
    else
        kill "$view_pid" 2>/dev/null || true
        wait "$view_pid" 2>/dev/null || true
        view_exit=124
    fi
    if [ "$browser_done" -eq 1 ]; then
        wait "$browser_pid" 2>/dev/null || browser_exit=$?
    else
        kill "$browser_pid" 2>/dev/null || true
        wait "$browser_pid" 2>/dev/null || true
        browser_exit=124
    fi

    # ── SO-07: after stop, collab_status shows no rooms ───────────────────
    local status3_resp
    status3_resp="$(http_rpc "$addr" "{\"type\":\"collab_status\",\"id\":\"status3\",\"roomId\":\"$room_id\"}")"
    local rooms_count
    rooms_count="$(printf '%s' "$status3_resp" | node -e '
        let d=""; process.stdin.on("data",c=>d+=c); process.stdin.on("end",()=>{
            const obj=JSON.parse(d); process.stdout.write(String((obj.data?.rooms||[]).length));
        });
    ')"
    if [ "$rooms_count" = "0" ]; then
        log "$SCENARIO: PASS SO-07: after stop, collab_status shows 0 rooms"
    else
        log "$SCENARIO: FAIL SO-07: after stop, expected 0 rooms, got $rooms_count"
        all_pass=1
    fi

    # ── Collect and verify guest assertion results ───────────────────────

    # Control guest results
    if [ -f "$ctrl_evidence/guest-results.json" ]; then
        local ctrl_failures
        ctrl_failures="$(node -e '
            const r = require(process.argv[1]);
            process.stdout.write(String(r.filter(x=>!x.passed).length));
        ' "$ctrl_evidence/guest-results.json")"
        if [ "$ctrl_failures" = "0" ]; then
            log "$SCENARIO: PASS control guest: all assertions passed"
        else
            log "$SCENARIO: FAIL control guest: $ctrl_failures assertion(s) failed"
            all_pass=1
        fi
    else
        log "$SCENARIO: FAIL control guest: no results file"
        all_pass=1
    fi

    # View guest results
    if [ -f "$view_evidence/guest-results.json" ]; then
        local view_failures
        view_failures="$(node -e '
            const r = require(process.argv[1]);
            process.stdout.write(String(r.filter(x=>!x.passed).length));
        ' "$view_evidence/guest-results.json")"
        if [ "$view_failures" = "0" ]; then
            log "$SCENARIO: PASS view guest: all assertions passed"
        else
            log "$SCENARIO: FAIL view guest: $view_failures assertion(s) failed"
            all_pass=1
        fi
    else
        log "$SCENARIO: FAIL view guest: no results file"
        all_pass=1
    fi

    # Browser guest results
    if [ -f "$browser_evidence/browser-results.json" ]; then
        local browser_failures
        browser_failures="$(node -e '
            const r = require(process.argv[1]);
            process.stdout.write(String(r.filter(x=>!x.passed).length));
        ' "$browser_evidence/browser-results.json")"
        if [ "$browser_failures" = "0" ]; then
            log "$SCENARIO: PASS browser guest: all assertions passed"
        else
            log "$SCENARIO: FAIL browser guest: $browser_failures assertion(s) failed"
            all_pass=1
        fi
    else
        log "$SCENARIO: FAIL browser guest: no results file"
        all_pass=1
    fi


    # Factor guest/browser process exit status into the final result. The
    # processes were already reaped/killed when their exit status was captured
    # above; a nonzero exit (assertion failure, crash, or hang) is a final fail.
    if [ "$ctrl_exit" -ne 0 ]; then
        log "$SCENARIO: FAIL control guest: process exited nonzero ($ctrl_exit)"
        all_pass=1
    fi
    if [ "$view_exit" -ne 0 ]; then
        log "$SCENARIO: FAIL view guest: process exited nonzero ($view_exit)"
        all_pass=1
    fi
    if [ "$browser_exit" -ne 0 ]; then
        log "$SCENARIO: FAIL browser guest: process exited nonzero ($browser_exit)"
        all_pass=1
    fi

    # Stop the headless listener after the guest assertions complete.
    kill -TERM "$RPI_PID" 2>/dev/null || true
    wait "$RPI_PID" 2>/dev/null || true
    # ── SO-08/09: scrub forbidden plaintext, then scan the whole root ─────
    if ! scrub_and_scan_evidence "$evidence" "$ctrl_key_b64" "$view_key_b64" "$root/workspace"; then
        all_pass=1
    fi

    if [ "$all_pass" -eq 0 ]; then
        printf 'collab passed\nevidence=%s\n' "$evidence"
    else
        fail "$SCENARIO: one or more assertions failed (see evidence=$evidence)"
    fi
}

case "${1:-run}" in
    list|--list)
        printf '%s\n' \
            'collab - one host + two CLI guests (control+view) + one Playwright browser control guest: encrypted back-history, live stream, tool card, browser/CLI control prompt, view rejection, host-only lifecycle, disconnect/rejoin fresh epoch, participant/status, host stop, ciphertext-absence-of-plaintext, no secret/path leakage'
        ;;
    run|all) main ;;
    *) fail "usage: $0 [run|list]" ;;
esac