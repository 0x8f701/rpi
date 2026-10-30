#!/usr/bin/env bash
set -euo pipefail
SCRIPT_DIR="$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
E2E_DIR="$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd -P)"
# shellcheck source=../lib/common.sh
. "$E2E_DIR/lib/common.sh"
# shellcheck source=../lib/orchestration_fixtures.sh
. "$E2E_DIR/lib/orchestration_fixtures.sh"

SCENARIO="code-review-ops"
PROVIDER="code-review-ops"
MODEL_ID="mock"
EARLY_MARKER="EARLY-HUNK-FLAG-0012"
MIDDLE_TAG="OPS-PADMIDDLE"
DEEP_MARKER="DEEP-HUNK-MARK-0240"
DEEP_SECOND="OPS-DEEP-NEXT-0241"
DEEP_HEADER='@@ -237,7 +1507,8 @@'
COMMENT_SENTINEL="OPS-COMMENT-SENTINEL-01"
SWITCH_MODEL="ops-switch-model"
AUTO_CARD="Auto review"
MANUAL_REPLY="MANUAL-REPLY-OPS-BODY"
CRITICAL_SUMMARY="Critical: deep marker has no guard"
CRITICAL_EVIDENCE="DEEP-HUNK-MARK-0240"
SECOND_SUMMARY="Naming: second deep row"
OVERDESIGN_SUMMARY="Overdesign: middle pad duplicates state"
NAMING_SUMMARY="Naming: deleted early baseline"
TLDR_OVERVIEW="Pinned lib.rs and z_big.md change the review base."

wait_for() { # needle [timeout]
    local needle="$1" timeout="${2:-25}"
    if ! tmux_wait_for "$session" "$timeout" "$needle" >"$evidence/live.txt"; then
        fail "user.$SCENARIO: did not find ${needle@Q} (see $evidence/live.txt)"
    fi
}

wait_all() { # timeout needle...
    local timeout="$1"
    shift
    if ! tmux_wait_for "$session" "$timeout" "$@" >"$evidence/live.txt"; then
        fail "user.$SCENARIO: did not find one of [$*] (see $evidence/live.txt)"
    fi
}

pane_capture() {
    tmux capture-pane -p -S -2000 -t "$session":0 2>/dev/null || fail "user.$SCENARIO: tmux pane capture failed"
}

capture_to() { # path
    pane_capture >"$1"
}

wait_gone() { # needle timeout label
    local needle="$1" timeout="$2" label="$3" deadline capture
    deadline=$((SECONDS + timeout))
    while ((SECONDS < deadline)); do
        capture="$(pane_capture)"
        case "$capture" in
            *"$needle"*) sleep 0.25 ;;
            *)
                printf '%s\n' "$capture" >"$evidence/gone-$label.txt"
                return 0
                ;;
        esac
    done
    capture_to "$evidence/gone-$label.txt"
    fail "user.$SCENARIO: ${needle@Q} never cleared (see $evidence/gone-$label.txt)"
}

assert_never_appears() { # label timeout needle...
    local label="$1" timeout="$2" deadline capture needle
    shift 2
    deadline=$((SECONDS + timeout))
    while ((SECONDS < deadline)); do
        capture="$(pane_capture)"
        for needle in "$@"; do
            case "$capture" in
                *"$needle"*)
                    printf '%s\n' "$capture" >"$evidence/never-$label.txt"
                    fail "user.$SCENARIO: ${needle@Q} appeared unexpectedly in the $label window (see $evidence/never-$label.txt)"
                    ;;
            esac
        done
        sleep 0.25
    done
    capture_to "$evidence/never-$label.txt"
}

release_gate() { # kind
    curl -s --max-time 5 -X POST "http://127.0.0.1:${mock_port}/__release-$1" \
        -d '{}' >"$evidence/release-$1.txt"
}

log_count() { # fixed-string -> occurrences in the mock log
    awk -v pattern="$1" 'index($0, pattern) {count++} END {print count + 0}' "$evidence/mock-server.log"
}

require_mock_count() { # fixed-string expected
    local got
    got="$(log_count "$1")"
    [ "$got" -eq "$2" ] || fail "user.$SCENARIO: mock log expected $2 of ${1@Q}, got $got (see $evidence/mock-server.log)"
}

wait_log() { # needle minimum-logged-count
    local needle="$1" want="$2" deadline
    deadline=$((SECONDS + 10))
    while ((SECONDS < deadline)); do
        if [ "$(log_count "$needle")" -ge "$want" ]; then
            return 0
        fi
        sleep 0.25
    done
    fail "user.$SCENARIO: mock log never reached $want of ${needle@Q} (see $evidence/mock-server.log)"
}

assert_mock_counts() {
    require_mock_count 'kind=triage' 2
    require_mock_count 'kind=auto' 1
    require_mock_count 'kind=highlight' 4
    require_mock_count 'kind=manual' 1
    require_mock_count 'kind=unrouted' 0
    require_mock_count 'strict=ok' 2
    require_mock_count 'findings-schema=ok' 4
    require_mock_count 'auto-schema=absent' 1
    require_mock_count 'lang=zh-CN' 5
    require_mock_count 'lang=missing' 0
    require_mock_count 'auto-scope=1' 1
    require_mock_count 'highlight-scope=complete' 4
    require_mock_count 'highlight-invalid-range' 1
    require_mock_count 'release=triage held=true' 2
    require_mock_count 'release=highlight held=true' 3
    require_mock_count 'release=auto held=true' 1
    require_mock_count 'held=false' 0
}

seed_review_fixture() { # root
    local root="$1" workspace="$1/workspace" status
    local gitenv=(env GIT_CONFIG_NOSYSTEM=1 GIT_CONFIG_GLOBAL="$root/absent-global-gitconfig")
    require_cmd git
    require_cmd python3
    "${gitenv[@]}" git -C "$workspace" init -q
    "${gitenv[@]}" git -C "$workspace" config user.email tui-e2e@example.com
    "${gitenv[@]}" git -C "$workspace" config user.name "TUI E2E"
    "${gitenv[@]}" git -C "$workspace" config commit.gpgsign false
    printf 'review root\n' >"$workspace/README.md"
    python3 - "$workspace/src/lib.rs" "$workspace/z_big.md" <<'PY'
import pathlib
import sys
base = "".join(f"LIB-BASE-{n:03d}\n" for n in range(1, 261))
pathlib.Path(sys.argv[1]).parent.mkdir(parents=True, exist_ok=True)
pathlib.Path(sys.argv[1]).write_text(base, encoding="utf-8")
pathlib.Path(sys.argv[2]).write_text("ZBIG-BASELINE\n", encoding="utf-8")
PY
    "${gitenv[@]}" git -C "$workspace" add -A
    "${gitenv[@]}" git -C "$workspace" commit -q -m "ops review base"
    "${gitenv[@]}" git -C "$workspace" branch -m review-base
    python3 - "$workspace/src/lib.rs" "$workspace/z_big.md" \
        "$EARLY_MARKER" "$MIDDLE_TAG" "$DEEP_MARKER" "OPS-DEEPPAD" "OPS-ZBIG" <<'PY'
import pathlib
import sys
lib, big = pathlib.Path(sys.argv[1]), pathlib.Path(sys.argv[2])
lines = [f"LIB-BASE-{n:03d}\n" for n in range(1, 261)]
lines[11] = sys.argv[3] + " unique-early-flag-line\n"
lines[239] = sys.argv[5] + " unique-deep-flag-line\n"
lines.insert(240, "OPS-DEEP-NEXT-0241 unique-deep-adjacent-line\n")
lines[30:210] = [f"{sys.argv[4]}-{i:04d} unique-middle-pad-row\n" for i in range(1450)]
lines.extend(f"{sys.argv[6]}-{i:05d} " + "p" * 78 + "\n" for i in range(30000))
lib.write_text("".join(lines), encoding="utf-8")
big.write_text(
    "".join(f"{sys.argv[7]}-{i:05d} " + "z" * 78 + "\n" for i in range(26000)),
    encoding="utf-8",
)
PY
    grep -F -q "$EARLY_MARKER" "$workspace/src/lib.rs" || fail "fixture: $EARLY_MARKER missing"
    grep -F -q "$DEEP_MARKER" "$workspace/src/lib.rs" || fail "fixture: $DEEP_MARKER missing"
    grep -F -q "$DEEP_SECOND" "$workspace/src/lib.rs" || fail "fixture: $DEEP_SECOND missing"
    [ "$(wc -c <"$workspace/z_big.md")" -gt 2000000 ] || fail "fixture: z_big.md too small for the truncation placeholder"
    status="$("${gitenv[@]}" git -C "$workspace" status --porcelain)"
    case "$status" in
        *" M src/lib.rs"*) ;;
        *) fail "fixture: src/lib.rs not modified vs review-base: ${status:-<empty>}" ;;
    esac
    case "$status" in
        *" M z_big.md"*) ;;
        *) fail "fixture: z_big.md not modified vs review-base: ${status:-<empty>}" ;;
    esac
}

write_ops_mock() { # root
    cat >"$1/ops_mock_server.py" <<'PY'
#!/usr/bin/env python3
"""Strict code-review-ops provider mock: content-routed SSE with release gates."""
import json
import re
import select
import socket
import sys
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

PORT_FILE = sys.argv[1]
GATE_LOCK = threading.Lock()
GATES = {}
SERIAL = 0
ZH_TAG = "Respond in the language tagged `zh-CN`."
AUTO_REPLY_ERROR = "ok"
HIGHLIGHT_SEEN = 0
TLDR_LATE = False
TLDR_OVERVIEW = "Pinned lib.rs and z_big.md change the review base."
TLDR_GOOD = (
    TLDR_OVERVIEW + "\n\n"
    "## Details\n\n"
    "- `src/lib.rs` keeps `" + "EARLY-HUNK-FLAG-0012" + "` and `" + "DEEP-HUNK-MARK-0240" + "`.\n"
    "- `z_big.md` grows past the review window.\n\n"
    "```mermaid\nflowchart LR\n  lib_rs --> z_big\n```\n"
)
TLDR_STALE = "STALE-LATE-SNAPSHOT must never paint.\n\nRejected after Stop.\n"
MANUAL_REPLY = "MANUAL-REPLY-OPS-BODY 人工复核：改动合理。\n"
COMMENT_SENTINEL = "OPS-COMMENT-SENTINEL-01"
IDENTITY_KEYS = ("snapshotId", "path", "oldStart", "oldCount", "newStart", "newCount", "contentHash")



def log(line):
    print(f"ops-mock {line}", file=sys.stderr, flush=True)


def register_gate(kind):
    gate = threading.Event()
    with GATE_LOCK:
        GATES[kind] = gate
    return gate


def pop_gate(kind):
    with GATE_LOCK:
        gate = GATES.pop(kind, None)
    if gate is not None:
        gate.set()
    return gate is not None


def sse(payload):
    return f"data: {json.dumps(payload, separators=(',', ':'))}\n\n"


def delta(tag, text, finish=None, model=None):
    choice = {"index": 0, "delta": {"content": text}}
    if finish is not None:
        choice["finish_reason"] = finish
    payload = {"id": tag, "choices": [choice]}
    if model is not None:
        payload["model"] = model
    return payload


def stream_text(handler, tag, text, model=None):
    body = sse(delta(tag, "", model=model)) + sse(delta(tag, text, finish="stop", model=model))
    handler.send_response(200)
    handler.send_header("content-type", "text/event-stream")
    handler.send_header("content-length", str(len(body)))
    handler.end_headers()
    handler.wfile.write(body.encode())
    handler.wfile.flush()


def peer_closed(handler):
    ready, _, _ = select.select([handler.connection], [], [], 0)
    if not ready:
        return False
    try:
        return handler.connection.recv(1, socket.MSG_PEEK) == b""
    except OSError:
        return True


def stream_held(handler, kind, final_text, model=None):
    gate = register_gate(kind)
    tag = f"ops-{kind}"
    handler.send_response(200)
    handler.send_header("content-type", "text/event-stream")
    handler.end_headers()
    try:
        handler.wfile.write(sse(delta(tag, "", model=model)).encode())
        handler.wfile.flush()
        for _ in range(1800):
            if peer_closed(handler):
                log(f"{kind} peer-closed before release")
                return
            if gate.wait(timeout=0.25):
                break
        else:
            log(f"{kind} hold expired")
            return
        handler.wfile.write(sse(delta(tag, final_text, finish="stop", model=model)).encode())
        handler.wfile.write(b"data: [DONE]\n\n")
        handler.wfile.flush()
        log(f"{kind} released")
    except OSError:
        log(f"{kind} aborted by client")


def full_text(body):
    chunks = []
    for message in body.get("messages", []):
        content = message.get("content")
        if isinstance(content, str):
            chunks.append(content)
        elif isinstance(content, list):
            for block in content:
                if isinstance(block, dict) and isinstance(block.get("text"), str):
                    chunks.append(block["text"])
    return "\n".join(chunks)


def prompt_identities(prompt):
    identities = []
    for line in prompt.splitlines():
        if not line.startswith("Hunk identity: "):
            continue
        fields = {}
        for field in line[len("Hunk identity: "):].split():
            key, separator, value = field.partition("=")
            if separator != "=":
                raise ValueError("identity field")
            fields[key] = value
        if tuple(fields) != IDENTITY_KEYS:
            raise ValueError("identity keys")
        for key in ("oldStart", "oldCount", "newStart", "newCount"):
            fields[key] = int(fields[key])
        if len(fields["contentHash"]) != 64:
            raise ValueError("content hash")
        identities.append(fields)
    return identities


def numbered_rows(prompt, identity):
    marker = (
        "Hunk identity: snapshotId={snapshotId} path={path} oldStart={oldStart} "
        "oldCount={oldCount} newStart={newStart} newCount={newCount} contentHash={contentHash}"
    ).format(**identity)
    start = prompt.index(marker)
    rows = []
    for line in prompt[start + len(marker):].splitlines()[1:]:
        if line.startswith("Hunk identity: ") or (line.startswith("File: ") and rows):
            break
        match = re.match(r"^\s*(\d+|-)\s+(\d+|-)\s+([ +\-@])(.*)$", line)
        if match:
            rows.append({
                "old": None if match.group(1) == "-" else int(match.group(1)),
                "new": None if match.group(2) == "-" else int(match.group(2)),
                "marker": match.group(3),
                "text": match.group(4),
            })
    return rows


def one_finding(category, side, line, summary, evidence):
    return {
        "category": category,
        "side": side,
        "startLine": line,
        "endLine": line,
        "summary": summary,
        "evidence": evidence,
    }


def findings_for(identity, rows):
    text = "\n".join(row["text"] for row in rows)
    if "DEEP-HUNK-MARK-0240" in text:
        deep = next(row["new"] for row in rows if row["marker"] == "+" and "DEEP-HUNK-MARK-0240" in row["text"])
        second = next(row["new"] for row in rows if row["marker"] == "+" and row["new"] != deep)
        return [
            one_finding("critical", "new", deep, "Critical: deep marker has no guard", "DEEP-HUNK-MARK-0240"),
            one_finding("naming", "new", second, "Naming: second deep row", next(row["text"].strip() for row in rows if row["new"] == second)),
        ]
    if "OPS-PADMIDDLE" in text:
        line = next(row["new"] for row in rows if row["marker"] == "+" and "OPS-PADMIDDLE" in row["text"])
        return [one_finding("overdesign", "new", line, "Overdesign: middle pad duplicates state", "OPS-PADMIDDLE")]
    if "EARLY-HUNK-FLAG-0012" in text:
        line = next(row["old"] for row in rows if row["marker"] == "-" and row["old"] is not None)
        evidence = next(row["text"].strip() for row in rows if row["old"] == line and row["marker"] == "-")
        return [one_finding("naming", "old", line, "Naming: deleted early baseline", evidence)]
    return []


def findings_document(prompt, require_complete):
    identities = prompt_identities(prompt)
    if require_complete and len(identities) < 2:
        raise ValueError("highlight scope")
    hunks = []
    for identity in identities:
        hunks.append({"identity": identity, "findings": findings_for(identity, numbered_rows(prompt, identity))})
    if require_complete:
        nonempty = [hunk for hunk in hunks if hunk["findings"]]
        if len(nonempty) != 3:
            raise ValueError("highlight findings")
    return {"hunks": hunks}



TRIAGE_SCHEMA = {
    "type": "object",
    "additionalProperties": False,
    "required": ["core", "skipped"],
    "properties": {
        "core": {"type": "array", "items": {"type": "string"}},
        "skipped": {"type": "integer", "minimum": 0},
    },
}


def assert_triage_schema(body):
    response_format = body.get("response_format") or {}
    schema = response_format.get("json_schema") or {}
    return (
        response_format.get("type") == "json_schema"
        and schema.get("name") == "review_triage"
        and schema.get("strict") is True
        and schema.get("schema") == TRIAGE_SCHEMA
        and "TRIAGE:" not in full_text(body)
    )


def triage_selection(text):
    """Strict review_triage body: one path@oldStart-newStart token for the deep hunk."""
    if "TRIAGE:" in text:
        raise ValueError("obsolete trailer")
    tokens = re.findall(r"^Hunk: (\S+)$", text, re.MULTILINE)
    if len(tokens) < 2 or len(tokens) != len(set(tokens)):
        raise ValueError("triage hunks")
    core = [token for token in tokens if "DEEP-HUNK-MARK-0240" in text.split(f"Hunk: {token}", 1)[1].split("\nHunk: ", 1)[0]]
    if len(core) != 1 or not re.fullmatch(r"\S+@\d+-\d+", core[0]):
        raise ValueError("deep token")
    return {"core": core, "skipped": len(tokens) - 1}

def schema_name(body):
    return ((body.get("response_format") or {}).get("json_schema") or {}).get("name")


def assert_findings_schema(body):
    response_format = body.get("response_format") or {}
    schema = (response_format.get("json_schema") or {})
    contract = response_format.get("type") == "json_schema" and schema.get("name") == "review_findings" and schema.get("strict") is True
    required = ((schema.get("schema") or {}).get("properties") or {}).get("hunks", {}).get("items", {}).get("properties", {}).get("identity", {}).get("required")
    return contract and required == list(IDENTITY_KEYS)


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.0"

    def do_GET(self):
        self.send_response(404)
        self.end_headers()

    def do_POST(self):
        try:
            length = int(self.headers.get("content-length", "0"))
            raw = self.rfile.read(length)
            if self.path.startswith("/__release-"):
                gate_map = {"triage": "triage", "highlight": "highlight", "auto": "auto", "tldr": "tldr"}
                kind = gate_map[self.path.rsplit("-", 1)[-1]]
                held = pop_gate(kind)
                log(f"release={kind} held={str(held).lower()}")
                body = b'{"released": %s}\n' % (b"true" if held else b"false")
                self.send_response(200)
                self.send_header("content-type", "application/json")
                self.send_header("content-length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)
                return
            body = json.loads(raw)
            text = full_text(body)
            if COMMENT_SENTINEL in text and "User comment:" in text:
                kind = "manual"
            elif "Classify every hunk" in text:
                kind = "triage"
            elif "Requested hunks:" in text and "Hunk identity:" in text:
                identities = prompt_identities(text)
                kind = "auto" if len(identities) == 1 else "highlight"
            elif text.startswith("Write one Markdown document about only the supplied current diff."):
                kind = "tldr"
            else:
                kind = "unrouted"
            with GATE_LOCK:
                type(self).bump_serial()
            log(f"request#{SERIAL} kind={kind} prompt_len={len(text)}")
            self.route(kind, body, text)
        except (BrokenPipeError, ConnectionResetError):
            log("client aborted mid-request")
        except Exception as error:
            log(f"request error={type(error).__name__}: {error}")
            try:
                stream_text(self, "ops-error", "mock rejected")
            except OSError:
                pass

    @classmethod
    def bump_serial(cls):
        global SERIAL
        SERIAL += 1

    def route(self, kind, body, text):
        tag = f"ops-{kind}"
        if kind == "triage":
            if not assert_triage_schema(body):
                log("triage strict=missing")
                stream_text(self, tag, '{"error": "mock requires strict review_triage schema"}')
                return
            selection = triage_selection(text)
            log("triage strict=ok")
            log(f"triage token={selection['core'][0]} skipped={selection['skipped']}")
            stream_held(self, kind, json.dumps(selection, separators=(',', ':')))
        elif kind == "auto":
            if ZH_TAG not in text:
                log("auto lang=missing")
                stream_text(self, tag, '{"error": "zh-CN review language tag missing"}')
                return
            if schema_name(body) not in (None, "review_findings"):
                log("auto-schema=unexpected")
                stream_text(self, tag, '{"error": "unexpected auto schema"}')
                return
            identities = prompt_identities(text)
            log("auto-schema=absent" if schema_name(body) is None else "auto-schema=review_findings")
            log("auto lang=zh-CN")
            log(f"auto-scope={len(identities)}")
            if AUTO_REPLY_ERROR == "range":
                document = findings_document(text, False)
                document["hunks"][0]["findings"][0]["endLine"] = identities[0]["newStart"] + identities[0]["newCount"] + 40
                stream_held(self, kind, json.dumps(document), model="ops-switch-model")
                return
            stream_held(self, kind, json.dumps(findings_document(text, False)), model="ops-switch-model")
        elif kind == "highlight":
            if not assert_findings_schema(body):
                log("findings-schema=missing")
                stream_text(self, tag, '{"error": "review_findings schema missing"}')
                return
            if ZH_TAG not in text:
                log("highlight lang=missing")
                stream_text(self, tag, '{"error": "zh-CN review language tag missing"}')
                return
            identities = prompt_identities(text)
            log("findings-schema=ok")
            log("highlight lang=zh-CN")
            log("highlight-scope=complete" if len(identities) >= 2 else "highlight-scope=short")
            document = findings_document(text, True)
            global HIGHLIGHT_SEEN
            HIGHLIGHT_SEEN += 1
            if HIGHLIGHT_SEEN >= 4:
                log("highlight-invalid-range")
                target = next(hunk for hunk in document["hunks"] if hunk["findings"])
                target["findings"][0]["endLine"] = target["identity"]["newCount"] + 80
                target["findings"][0]["summary"] = "STALE-RANGE"
            stream_held(self, kind, json.dumps(document))
        elif kind == "tldr":
            schema_absent = body.get("response_format") is None and "tools" not in body
            log("tldr-schema=absent" if body.get("response_format") is None else "tldr-schema=present")
            log("tldr-tools=absent" if not body.get("tools") else "tldr-tools=present")
            if ZH_TAG not in text or not schema_absent:
                log("tldr lang=missing")
                stream_text(self, tag, "TLDR contract missing")
                return
            log("tldr lang=zh-CN")
            if "comparisonLabel:" not in text or "src/lib.rs" not in text:
                stream_text(self, tag, "TLDR scope missing")
                return
            stream_held(self, kind, TLDR_STALE if TLDR_LATE else TLDR_GOOD)
        elif kind == "manual":
            stream_text(self, tag, MANUAL_REPLY, model="ops-switch-model")
        else:
            stream_text(self, tag, '{"error": "mock unrouted"}')


def main():
    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    with open(PORT_FILE, "w", encoding="utf-8") as handle:
        handle.write(str(server.server_port))
    log(f"listening port={server.server_port}")
    server.serve_forever()


if __name__ == "__main__":
    main()
PY
}

start_ops_mock() { # root -> prints port
    local root="$1" port_file="$root/mock-port.txt" deadline port
    python3 "$root/ops_mock_server.py" "$port_file" >"$evidence/mock-server.log" 2>&1 &
    register_pid $!
    deadline=$((SECONDS + 15))
    while [ ! -s "$port_file" ] && [ "$SECONDS" -lt "$deadline" ]; do sleep 0.2; done
    [ -s "$port_file" ] || fail "user.$SCENARIO: ops mock did not write its port file"
    port="$(cat "$port_file")"
    printf '%s\n' "$port"
}

main() {
    require_rpi
    require_cmd tmux
    require_cmd git
    require_cmd python3
    require_cmd curl

    local root evidence session port size
    root="$(scenario_workspace "$SCENARIO")"
    evidence="$EVIDENCE_ROOT/$SCENARIO"
    session="$(unique_tmux_name "$SCENARIO")"
    register_tmux_session "$session"
    seed_review_fixture "$root"
    write_ops_mock "$root"
    port="$(start_ops_mock "$root")"
    mock_port="$port"

    mkdir -p "$root/home/.pi/agent"
    for settings in "$root/home/.pi/agent/settings.json" "$root/home/.rpi/settings.json"; do
        printf '{\n  "reviewLanguage": "zh-CN"\n}\n' >"$settings"
    done
    cat >"$root/home/.pi/agent/models.json" <<EOF
{
  "providers": {
    "$PROVIDER": {
      "baseUrl": "http://127.0.0.1:$port",
      "api": "openai-completions",
      "models": [
        {
          "id": "$MODEL_ID",
          "name": "Ops Review Mock",
          "contextWindow": 1000000,
          "maxTokens": 8192,
          "compat": { "supportsResponseSchema": true }
        }
      ]
    }
  }
}
EOF

    # tmux 3.4 default window-size=latest ignores new-session -x/-y for the
    # live window (this server: latest, observed 40x17 from the newest client;
    # -x/-y only sets session default-size). resize-window pins this window to
    # manual 140x42 before rpi reads the pty. Global window-size stays latest.
    tmux new-session -d -s "$session" -x 140 -y 42 -c "$root/workspace" \
        'sleep 86400' \
        \; set-option -t "$session" remain-on-exit on \
        \; resize-window -t "$session":0 -x 140 -y 42
    size="$(tmux display-message -p -t "$session":0 '#{window_width}x#{window_height}')"
    [ "$size" = "140x42" ] || fail "user.$SCENARIO: pane is ${size}, expected 140x42"
    tmux respawn-pane -k -c "$root/workspace" -t "$session":0 \
        "env HOME='$root/home' USERPROFILE='$root/home' \
PI_CODING_AGENT_DIR='$root/home/.pi/agent' \
RPI_OFFLINE=1 RPI_SKIP_VERSION_CHECK=1 \
TERM=xterm-256color \
'$RPI_BIN' --offline --model $PROVIDER/$MODEL_ID --api-key ops-mock-key; printf '===TUI-DONE-code-review-operations===\n'"

    wait_for "$PROVIDER/$MODEL_ID" 30
    capture_to "$evidence/00-boot.txt"
    tmux send-keys -t "$session":0 Escape
    sleep 0.3

    tmux send-keys -t "$session":0 -l '/code-review review-base'
    wait_for '/code-review review-base' 10
    tmux send-keys -t "$session":0 Enter
    wait_all 45 'Code review · review-base → working tree · 2 files' \
        'focus:tree · A Auto · H Highlight' \
        'lib.rs' 'z_big.md' '@@' "$EARLY_MARKER"
    capture_to "$evidence/01-open.txt"
    assert_file_contains "$evidence/01-open.txt" \
        'Code review · review-base → working tree · 2 files' \
        'focus:tree · A Auto · H Highlight' \
        'lib.rs' 'z_big.md' '@@' "$EARLY_MARKER"
    assert_file_lacks "$evidence/01-open.txt" "$DEEP_MARKER" '· on demand' '■ Stop'

    tmux send-keys -t "$session":0 Left
    wait_for 'Select a file' 10
    tmux send-keys -t "$session":0 Left
    wait_gone 'lib.rs' 10 fold-collapse
    capture_to "$evidence/02-folded.txt"
    assert_file_contains "$evidence/02-folded.txt" 'z_big.md'
    assert_file_lacks "$evidence/02-folded.txt" 'lib.rs' "$EARLY_MARKER"

    tmux send-keys -t "$session":0 Right
    wait_for 'lib.rs' 10
    capture_to "$evidence/03-expanded.txt"
    assert_file_contains "$evidence/03-expanded.txt" 'lib.rs' 'z_big.md'
    assert_file_lacks "$evidence/03-expanded.txt" "$EARLY_MARKER"

    tmux send-keys -t "$session":0 Down
    wait_for "$EARLY_MARKER" 10
    capture_to "$evidence/03b-refocused.txt"
    assert_file_contains "$evidence/03b-refocused.txt" 'lib.rs' "$EARLY_MARKER" 'z_big.md'

    tmux send-keys -t "$session":0 Down
    wait_for '· on demand' 10
    capture_to "$evidence/04-on-demand.txt"
    assert_file_contains "$evidence/04-on-demand.txt" 'z_big.md' '· on demand'
    assert_file_lacks "$evidence/04-on-demand.txt" "$DEEP_MARKER"

    tmux send-keys -t "$session":0 Up
    wait_for "$EARLY_MARKER" 10
    capture_to "$evidence/05-before-jump.txt"
    assert_file_contains "$evidence/05-before-jump.txt" 'lib.rs' "$EARLY_MARKER"
    assert_file_lacks "$evidence/05-before-jump.txt" "$DEEP_MARKER"

    tmux send-keys -t "$session":0 ']'
    tmux send-keys -t "$session":0 ']'
    wait_all 15 'focus:diff' "$DEEP_HEADER"
    capture_to "$evidence/06-deep-jump-header.txt"
    assert_file_contains "$evidence/06-deep-jump-header.txt" 'focus:diff' "$DEEP_HEADER"
    tmux send-keys -t "$session":0 PageDown
    wait_all 15 'focus:diff' "$DEEP_MARKER"
    capture_to "$evidence/06-deep-jump.txt"
    assert_file_contains "$evidence/06-deep-jump.txt" 'focus:diff' "$DEEP_MARKER"

    tmux send-keys -t "$session":0 a
    wait_for 'Auto Review triaging ■ Stop' 20
    capture_to "$evidence/07-triage-held.txt"
    assert_file_contains "$evidence/07-triage-held.txt" 'Auto Review triaging ■ Stop'

    tmux send-keys -t "$session":0 a
    wait_for 'Auto Review stopped' 15
    wait_gone '■ Stop' 15 auto-stopped
    capture_to "$evidence/08-auto-stopped.txt"
    assert_file_contains "$evidence/08-auto-stopped.txt" 'Auto Review stopped' 'Code review'

    tmux send-keys -t "$session":0 h
    wait_for 'Highlight ■ Stop' 15
    capture_to "$evidence/09-highlight-held.txt"
    assert_file_contains "$evidence/09-highlight-held.txt" 'Highlight ■ Stop' 'lib.rs'

    tmux send-keys -t "$session":0 Escape
    wait_for 'Highlight stopped' 15
    wait_gone '■ Stop' 15 esc-precedence
    capture_to "$evidence/10-esc-precedence.txt"
    assert_file_contains "$evidence/10-esc-precedence.txt" 'Highlight stopped' 'Code review'

    wait_log 'kind=triage' 1
    wait_log 'kind=highlight' 1
    release_gate triage
    release_gate highlight
    assert_never_appears late-no-revival 5 'Auto Review triaging' 'Highlight ■ Stop' '· Critical ·'

    tmux send-keys -t "$session":0 a
    wait_for 'Auto Review triaging ■ Stop' 20
    wait_log 'kind=triage' 2
    release_gate triage
    wait_for 'Auto Review 0/1 ■ Stop' 20
    capture_to "$evidence/11-deep-held.txt"
    assert_file_contains "$evidence/11-deep-held.txt" 'Auto Review 0/1 ■ Stop'
    wait_log 'kind=auto' 1
    release_gate auto
    wait_all 25 'Auto review' 'Answer 1 · ops-switch-model' "$CRITICAL_SUMMARY"
    capture_to "$evidence/12-auto-card.txt"
    assert_file_contains "$evidence/12-auto-card.txt" \
        'Auto review' 'Answer 1 · ops-switch-model' "$CRITICAL_SUMMARY" "$CRITICAL_EVIDENCE"
    assert_file_lacks "$evidence/12-auto-card.txt" 'REVIEW-VERDICT' 'severity=' 'Key changes' 'warning' 'info'

    tmux send-keys -t "$session":0 '['
    wait_for '@@ -28,186 +28,1456 @@' 10
    tmux send-keys -t "$session":0 ']'
    wait_for "$DEEP_HEADER" 10
    tmux send-keys -t "$session":0 h
    wait_for 'Highlight ■ Stop' 15
    wait_log 'kind=highlight' 2
    release_gate highlight
    # Auto publishes only the deep hunk, whose header is already
    # "Critical · 2 findings". Highlight completion sets Done and publishes
    # the other hunks without changing the footer. '[' returns to the early
    # hunk; its "Naming · 1 finding" chip exists only after that publish.
    tmux send-keys -t "$session":0 '['
    tmux send-keys -t "$session":0 '['
    wait_for 'Naming · 1 finding' 20
    # Highlight publishes findings without selecting one. n/p walk snapshot
    # order: files, hunks, then row order inside a hunk. That is early naming,
    # middle overdesign, then the deep hunk's critical row before its adjacent
    # naming row. Extra n/p stay on the endpoint.
    tmux send-keys -t "$session":0 n
    wait_for "Finding 1 of 4 · naming · $NAMING_SUMMARY" 20
    capture_to "$evidence/13-finding-early-naming.txt"
    assert_file_contains "$evidence/13-finding-early-naming.txt" \
        "Finding 1 of 4 · naming · $NAMING_SUMMARY" "$EARLY_MARKER" '!'
    assert_file_lacks "$evidence/13-finding-early-naming.txt" 'Key changes' 'warning' 'info'

    tmux send-keys -t "$session":0 n
    wait_for "Finding 2 of 4 · overdesign · $OVERDESIGN_SUMMARY" 10
    capture_to "$evidence/13b-finding-middle-overdesign.txt"
    assert_file_contains "$evidence/13b-finding-middle-overdesign.txt" \
        "Finding 2 of 4 · overdesign · $OVERDESIGN_SUMMARY" "$MIDDLE_TAG"

    tmux send-keys -t "$session":0 n
    wait_for "Finding 3 of 4 · critical · $CRITICAL_SUMMARY" 10
    capture_to "$evidence/13c-finding-deep-critical.txt"
    assert_file_contains "$evidence/13c-finding-deep-critical.txt" \
        "Finding 3 of 4 · critical · $CRITICAL_SUMMARY" "$DEEP_MARKER" '!'

    tmux send-keys -t "$session":0 n
    wait_for "Finding 4 of 4 · naming · $SECOND_SUMMARY" 10
    capture_to "$evidence/14-finding-deep-naming.txt"
    assert_file_contains "$evidence/14-finding-deep-naming.txt" \
        "Finding 4 of 4 · naming · $SECOND_SUMMARY" "$DEEP_SECOND" "$DEEP_MARKER"
    tmux send-keys -t "$session":0 n
    capture_to "$evidence/14b-next-endpoint.txt"
    assert_file_contains "$evidence/14b-next-endpoint.txt" "Finding 4 of 4 · naming · $SECOND_SUMMARY"
    assert_file_lacks "$evidence/14b-next-endpoint.txt" "Finding 1 of 4" "Finding 3 of 4"

    tmux send-keys -t "$session":0 p
    wait_for "Finding 3 of 4 · critical · $CRITICAL_SUMMARY" 10
    tmux send-keys -t "$session":0 p
    tmux send-keys -t "$session":0 p
    wait_for "Finding 1 of 4 · naming · $NAMING_SUMMARY" 10
    tmux send-keys -t "$session":0 p
    capture_to "$evidence/14c-previous-endpoint.txt"
    assert_file_contains "$evidence/14c-previous-endpoint.txt" "Finding 1 of 4 · naming · $NAMING_SUMMARY"
    assert_file_lacks "$evidence/14c-previous-endpoint.txt" "Finding 4 of 4" "Finding 2 of 4"
    tmux send-keys -t "$session":0 h
    wait_for 'Highlight ■ Stop' 15
    tmux send-keys -t "$session":0 h
    wait_for 'Highlight stopped' 15
    wait_gone '■ Stop' 15 highlight-stopped
    capture_to "$evidence/15-highlight-stopped.txt"
    assert_file_contains "$evidence/15-highlight-stopped.txt" 'Highlight stopped' 'Code review'

    tmux send-keys -t "$session":0 h
    wait_for 'Highlight ■ Stop' 15
    wait_log 'kind=highlight' 4
    release_gate highlight
    wait_for 'Highlight failed' 20
    capture_to "$evidence/15b-highlight-rejected.txt"
    assert_file_contains "$evidence/15b-highlight-rejected.txt" \
        'Highlight failed' 'range is not a contiguous numbered side of this hunk'
    assert_file_lacks "$evidence/15b-highlight-rejected.txt" 'STALE-RANGE'
    # p rewrites the footer with the retained finding 1, so the range error
    # is evidence only in the capture above.
    tmux send-keys -t "$session":0 p
    wait_for "Finding 1 of 4 · naming · $NAMING_SUMMARY" 10
    capture_to "$evidence/15b-retained-finding.txt"
    assert_file_contains "$evidence/15b-retained-finding.txt" \
        "Finding 1 of 4 · naming · $NAMING_SUMMARY" "$EARLY_MARKER" '!'
    tmux send-keys -t "$session":0 n
    wait_for "Finding 2 of 4 · overdesign · $OVERDESIGN_SUMMARY" 10
    tmux send-keys -t "$session":0 n
    wait_for "Finding 3 of 4 · critical · $CRITICAL_SUMMARY" 10
    tmux send-keys -t "$session":0 n
    wait_for "Finding 4 of 4 · naming · $SECOND_SUMMARY" 10
    capture_to "$evidence/15c-last-good-findings.txt"
    assert_file_contains "$evidence/15c-last-good-findings.txt" \
        "Finding 4 of 4 · naming · $SECOND_SUMMARY" "$DEEP_SECOND" '!'
    tmux send-keys -t "$session":0 n
    capture_to "$evidence/15d-next-endpoint.txt"
    assert_file_contains "$evidence/15d-next-endpoint.txt" "Finding 4 of 4 · naming · $SECOND_SUMMARY"
    tmux send-keys -t "$session":0 p
    tmux send-keys -t "$session":0 p
    tmux send-keys -t "$session":0 p
    wait_for "Finding 1 of 4 · naming · $NAMING_SUMMARY" 10
    tmux send-keys -t "$session":0 p
    capture_to "$evidence/15e-previous-endpoint.txt"
    assert_file_contains "$evidence/15e-previous-endpoint.txt" "Finding 1 of 4 · naming · $NAMING_SUMMARY"

    # The endpoint check leaves the early hunk selected. Return to the deep
    # thread before the comment composer, or c opens Comment 1 there.
    tmux send-keys -t "$session":0 ']'
    tmux send-keys -t "$session":0 ']'
    wait_for "$DEEP_HEADER" 10
    tmux send-keys -t "$session":0 c
    wait_for ' Comment · Enter submit · Esc cancel ' 10
    tmux send-keys -t "$session":0 -l "$COMMENT_SENTINEL"
    wait_for "$COMMENT_SENTINEL" 10
    capture_to "$evidence/16-composer.txt"
    assert_file_contains "$evidence/16-composer.txt" ' Comment · Enter submit · Esc cancel ' "$COMMENT_SENTINEL"

    tmux send-keys -t "$session":0 Escape
    wait_gone 'Enter submit' 10 composer-cancel
    capture_to "$evidence/17-composer-cancel.txt"
    assert_file_lacks "$evidence/17-composer-cancel.txt" 'Enter submit' "$COMMENT_SENTINEL"
    assert_file_contains "$evidence/17-composer-cancel.txt" 'Code review'

    tmux send-keys -t "$session":0 c
    wait_for ' Comment · Enter submit · Esc cancel ' 10
    tmux send-keys -t "$session":0 -l "$COMMENT_SENTINEL"
    tmux send-keys -t "$session":0 Enter
    wait_all 25 'Comment 2' "$MANUAL_REPLY" 'Answer 2 · ops-switch-model'
    capture_to "$evidence/18-manual-comment.txt"
    assert_file_contains "$evidence/18-manual-comment.txt" 'Comment 2' "$MANUAL_REPLY" 'Answer 2 · ops-switch-model'

    tmux send-keys -t "$session":0 Space
    wait_for '· folded' 10
    capture_to "$evidence/19-folded.txt"
    assert_file_contains "$evidence/19-folded.txt" '· 4 comments · folded'

    tmux send-keys -t "$session":0 q
    wait_gone 'review-base → working tree' 15 closed
    capture_to "$evidence/20-closed.txt"
    assert_file_lacks "$evidence/20-closed.txt" 'review-base → working tree' 'focus:tree' 'focus:diff'

    tmux send-keys -t "$session":0 -l '/quit'
    wait_for '/quit' 10
    tmux send-keys -t "$session":0 Enter
    if ! tmux_wait_for "$session" 20 '===TUI-DONE-code-review-operations===' >"$evidence/21-quit.txt"; then
        fail "user.$SCENARIO: TUI did not exit after /quit (see $evidence/21-quit.txt)"
    fi

    assert_mock_counts
    printf 'user.%s passed\nevidence=%s\n' "$SCENARIO" "$evidence"
}

main "$@"