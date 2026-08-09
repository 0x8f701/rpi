#!/usr/bin/env bash
set -euo pipefail
SCRIPT_DIR="$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
. "$SCRIPT_DIR/../lib/common.sh"
case "${1:-list}" in
    list|--list|--dry-run) printf '%s\n' 'release.listen-web - closed-stdin Web-only listener, tokenless RPC, signal shutdown, and recorded conversation restart'; exit 0 ;;
    run) ;;
    *) fail "usage: $0 [run|list|--dry-run]" ;;
esac
prepare_roots; require_rpi; require_cmd python3
root="$(scenario_workspace release-listen-web)"
evidence="$EVIDENCE_ROOT/release-listen-web"
session_dir="$root/sessions"
mkdir -p "$session_dir"
port_file="$root/port"
python3 - "$port_file" <<'PY'
import socket, sys
sock = socket.socket()
sock.bind(("127.0.0.1", 0))
open(sys.argv[1], "w", encoding="utf-8").write(str(sock.getsockname()[1]))
sock.close()
PY
port="$(cat "$port_file")"
start_listener() {
    local tag="$1"; shift
    env -i HOME="$root/home" USERPROFILE="$root/home" PATH="${PATH:-/usr/bin:/bin}" \
        LANG="${LANG:-C.UTF-8}" LC_ALL="${LC_ALL:-C.UTF-8}" TERM="xterm-256color" \
        PI_CODING_AGENT_DIR="$root/home/.pi/agent" PI_OFFLINE=1 PI_SKIP_VERSION_CHECK=1 \
        PI_FAUX_RESPONSE='release Web persisted reply' \
        "$RPI_BIN" --offline -C "$root/workspace" --listen "127.0.0.1:$port" \
        --model faux/faux-1 --api-key faux --session-dir "$session_dir" "$@" \
        </dev/null >"$evidence/stdout$tag.log" 2>"$evidence/stderr$tag.log" &
    RPI_PID=$!
    register_pid "$RPI_PID"
}
rpc() {
    local payload="$1" output="$2"
    python3 - "$port" "$payload" "$output" <<'PY'
import json, socket, sys
port = int(sys.argv[1]); payload = sys.argv[2].encode(); output = sys.argv[3]
request = (f"POST /rpc HTTP/1.1\r\nhost: 127.0.0.1:{port}\r\ncontent-type: application/json\r\ncontent-length: {len(payload)}\r\nconnection: close\r\n\r\n").encode() + payload
with socket.create_connection(("127.0.0.1", port), timeout=2) as sock:
    sock.settimeout(30); sock.sendall(request); response = b""
    while True:
        chunk = sock.recv(65536)
        if not chunk: break
        response += chunk
body = response.split(b"\r\n\r\n", 1)[1]
value = json.loads(body)
open(output, "w", encoding="utf-8").write(json.dumps(value, sort_keys=True))
if not value.get("success"): raise SystemExit(value)
PY
}
wait_ready() {
    local deadline=$((SECONDS + 30))
    while [ "$SECONDS" -lt "$deadline" ]; do
        if rpc '{"type":"get_state","id":"ready"}' "$evidence/ready.json" 2>/dev/null; then return 0; fi
        kill -0 "$RPI_PID" 2>/dev/null || fail "listener exited before readiness"
        sleep 0.05
    done
    fail "listener readiness timed out"
}
assert_headless() {
    local tag="$1" combined
    combined="$root/combined$tag.log"
    cat "$evidence/stdout$tag.log" "$evidence/stderr$tag.log" >"$combined"
    python3 - "$combined" <<'PY'
import sys
raw = open(sys.argv[1], "rb").read()
for marker in [b"\x1b[6n", b"\x1b[?25l", b"\x1b[?1049h", b"Recent sessions", b"cursor position could not be read"]:
    if marker in raw: raise SystemExit(f"forbidden TUI marker: {marker!r}")
if b"Control plane listening" not in raw: raise SystemExit("startup URL missing")
PY
}
start_listener 1
wait_ready
kill -0 "$RPI_PID" 2>/dev/null || fail "closed stdin stopped listener"
python3 - "$port" "$evidence/web.html" <<'PY'
import socket, sys
port = int(sys.argv[1]); output = sys.argv[2]
request = f"GET /web HTTP/1.1\r\nhost: 127.0.0.1:{port}\r\nconnection: close\r\n\r\n".encode()
with socket.create_connection(("127.0.0.1", port), timeout=2) as sock:
    sock.sendall(request); response = b""
    while True:
        chunk = sock.recv(65536)
        if not chunk: break
        response += chunk
if not response.startswith(b"HTTP/1.1 200") or b"<!doctype html" not in response: raise SystemExit("/web unavailable")
open(output, "wb").write(response)
PY
prompt='release Web prompt survives restart'
rpc "{\"type\":\"prompt\",\"id\":\"prompt\",\"message\":\"$prompt\"}" "$evidence/prompt.json"
deadline=$((SECONDS + 30))
while :; do
    rpc '{"type":"get_entries","id":"entries"}' "$evidence/entries-before.json"
    if python3 - "$evidence/entries-before.json" "$prompt" <<'PY'
import sys
text = open(sys.argv[1], encoding="utf-8").read()
raise SystemExit(0 if sys.argv[2] in text and "release Web persisted reply" in text else 1)
PY
    then break; fi
    [ "$SECONDS" -lt "$deadline" ] || fail "recorded Web turn timed out"
    sleep 0.05
done
kill -TERM "$RPI_PID"
wait "$RPI_PID"
assert_headless 1
start_listener 2 --continue
wait_ready
rpc '{"type":"get_entries","id":"restored"}' "$evidence/entries-after.json"
python3 - "$evidence/entries-after.json" "$prompt" <<'PY'
import sys
text = open(sys.argv[1], encoding="utf-8").read()
if sys.argv[2] not in text or "release Web persisted reply" not in text: raise SystemExit("recorded conversation was not restored")
PY
kill -TERM "$RPI_PID"
wait "$RPI_PID"
assert_headless 2
printf 'release Web listener smoke passed\nevidence=%s\n' "$evidence"
