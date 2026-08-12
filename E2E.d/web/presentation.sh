#!/usr/bin/env bash
# Web client presentation E2E lane (playwright-only hard gate) —
# real Chromium regressions for the tool-card / Command / process / write /
# read presentation, the Thinking streaming lifecycle, the composer control
# equal-height, the session sidebar provider grouping + search, and the
# inline image / video media contracts.
#
# Spawns the real `rpi --listen` binary with a token file and the loopback
# steering mock (MOCK_REASONING=1 so reasoning_content deltas parse), enables
# orchestration.process+tasks (the `process` tool must be registered for the
# process cards), seeds deterministic media fixtures (a valid PNG, a WebM
# EBML header, a hostile SVG) and foreign OMP/Codex/Grok + native sessions
# under the isolated HOME, then opens /web in a real browser and runs
# presentation_test.mjs. The mock seeds are content-routed by EXACT prompt
# markers added additively to E2E.d/lib/user_mock_server.py (steering
# scenario); the durable bashExecution path is driven via a second WS RPC.
#
# Browser driver: playwright via npm (ephemeral install) with a system
# Chrome/Chromium binary or playwright's bundled chromium. Missing node, a
# failed playwright install, or no usable Chromium FAILS the lane (exit 1,
# setup); assertion failures exit 2+.
#
# Usage: bash E2E.d/web/presentation.sh [run|list]
set -euo pipefail
SCRIPT_DIR="$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
E2E_DIR="$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd -P)"
# shellcheck source=../lib/common.sh
. "$E2E_DIR/lib/common.sh"
# shellcheck source=lib/fixture.sh
. "$SCRIPT_DIR/lib/fixture.sh"

SCENARIO="web-presentation"
TOKEN="web-presentation-e2e-token-$$-$(date +%s)"

# ---------------------------------------------------------------------------
main() {
    case "${1:-run}" in
        list|--list|--dry-run)
            printf '%s\n' \
                'web-presentation - tool-card presentation (Command/process/write/read titles, two-line clamp, no raw args, success-green/failure-red border), process equal width + .op error, thinking streaming-visible/final-hidden, composer equal height (desktop+mobile), durable bashExecution, session provider grouping + search, inline image render, video controls/preload/no-autoplay, hostile media rejected (PLAYWRIGHT-ONLY, hard-fail)'
            return 0
            ;;
        run|all) ;;
        *) fail "usage: $0 [run|list]" ;;
    esac

    require_rpi
    require_cmd node
    require_cmd python3

    local root evidence port url pw_status=0
    root="$(scenario_workspace "$SCENARIO")"
    evidence="$EVIDENCE_ROOT/$SCENARIO"
    printf '%s\n' "$TOKEN" >"$root/token"

    # Reasoning model so thinking_delta (reasoning_content) is requested/parsed.
    MOCK_REASONING=1 port="$(web_start_mock "$root" "$evidence")"

    # Orchestration: tasks (shared with core) + process (the `process` tool
    # must be registered so the process tool_calls execute, not error as
    # "Tool process not found"). No sessionImportSources → Web default
    # NativePi+OMP+Codex+Grok discovery.
    cat >"$root/home/.pi/agent/settings.json" <<EOF
{
  "orchestration": {
    "tasks": true,
    "process": true,
    "maxConcurrency": 2,
    "maxRecursionDepth": 2
  }
}
EOF

    # Seed workspace: git repo + deterministic media fixtures + foreign sessions.
    require_cmd git
    git -C "$root/workspace" init -q
    git -C "$root/workspace" config user.email "web-presentation@example.com"
    git -C "$root/workspace" config user.name "Web Presentation"
    git -C "$root/workspace" config commit.gpgsign false
    printf 'web presentation seed\n' >"$root/workspace/seed.txt"
    git -C "$root/workspace" add -- seed.txt
    git -C "$root/workspace" -c commit.gpgsign=false commit -q -m seed

    python3 - "$root/workspace" "$root/home" "$root/workspace" <<'PY'
import pathlib, struct, sys, zlib

ws = pathlib.Path(sys.argv[1])
home = pathlib.Path(sys.argv[2])
cwd = sys.argv[3]

# Minimal valid 16x16 solid PNG (std lib only: raw PNG chunk encoding).
def png_chunk(kind: bytes, data: bytes) -> bytes:
    return struct.pack(">I", len(data)) + kind + data + struct.pack(">I", zlib.crc32(kind + data) & 0xFFFFFFFF)

def solid_png(width: int, height: int, rgb: tuple[int, int, int]) -> bytes:
    sig = b"\x89PNG\r\n\x1a\n"
    ihdr = struct.pack(">IIBBBBB", width, height, 8, 2, 0, 0, 0)  # 8-bit RGB
    row = b"\x00" + bytes(rgb) * width
    idat = zlib.compress(b"".join([row] * height), 9)
    return sig + png_chunk(b"IHDR", ihdr) + png_chunk(b"IDAT", idat) + png_chunk(b"IEND", b"")

(ws / "logo.png").write_bytes(solid_png(16, 16, (30, 120, 70)))

# Minimal WebM EBML header (magic 0x1A45DFA3 + doctype "webm") so a
# magic-byte/extension video detector recognises the file. A full container
# is not required for the read tool's media detection.
def vint_one(value: int) -> bytes:
    return bytes([0x80 | value])
children = (
    b"\x42\x86\x81\x01"      # EBMLVersion = 1
    b"\x42\xf7\x81\x01"      # EBMLReadVersion = 1
    b"\x42\xf2\x81\x04"      # EBMLMaxIDLength = 4
    b"\x42\xf3\x81\x08"      # EBMLMaxSizeLength = 8
    b"\x42\x82\x84webm"      # DocType = "webm"
    b"\x42\x87\x81\x04"      # DocTypeVersion = 4
    b"\x42\x85\x81\x02"      # DocTypeReadVersion = 2
)
ebml_header = b"\x1a\x45\xdf\xa3" + vint_one(len(children)) + children
(ws / "capture.webm").write_bytes(ebml_header)

# Hostile SVG: an unsupported image MIME (the read allowlist excludes SVG) →
# the renderer must not produce an inline media element.
(ws / "hostile.svg").write_text(
    "<?xml version=\"1.0\"?>\n<svg xmlns=\"http://www.w3.org/2000/svg\""
    " width=\"10\" height=\"10\"><rect width=\"10\" height=\"10\"/></svg>\n",
    encoding="utf-8",
)

# Foreign + native sessions under the isolated HOME (mirrors
# SessionCatalog::from_env discovery): one native rpi row + OMP + Codex + Grok
# so the sidebar shows exactly the rpi/Codex/Grok/OMP provider groups.
import datetime, json
agent = home / ".pi" / "agent"
sessions_root = agent / "sessions"
def encoded(path: str) -> str:
    s = path[1:] if path.startswith("/") else path
    return "--" + s.replace("/", "-").replace("\\", "-").replace(":", "-") + "--"
def write_text(p: pathlib.Path, body: str) -> None:
    p.parent.mkdir(parents=True, exist_ok=True)
    p.write_text(body, encoding="utf-8")
stamp = datetime.datetime.now(datetime.timezone.utc).isoformat().replace("+00:00", "Z")
native_dir = sessions_root / encoded(cwd)
native_dir.mkdir(parents=True, exist_ok=True)
native_sid = "native-pres-seed"
native_file = native_dir / f"{stamp[:10]}T000000-{native_sid}.jsonl"
native_records = [
    {"type": "session", "version": 3, "id": native_sid, "timestamp": stamp, "cwd": cwd},
    # Explicit session_info name: the Web sidebar `temporary` marker must
    # never apply to this legal native seed (native + unnamed + <10KiB + cwd
    # under the OS temp root would otherwise hide it by default).
    {"type": "session_info", "id": "si-1", "parentId": None, "timestamp": stamp,
     "name": native_sid},
    {"type": "message", "id": "m1", "parentId": None, "timestamp": stamp,
     "message": {"role": "user", "content": [{"type": "text", "text": "native-pres-seed"}], "timestamp": 1}},
]
native_file.write_text("".join(json.dumps(r, separators=(",", ":")) + "\n" for r in native_records), encoding="utf-8")

omp_id = "omp-pres-e2e"
write_text(home / ".omp" / "agent" / "sessions" / "--work--" / f"{omp_id}.jsonl",
    json.dumps({"type": "session", "version": 3, "id": omp_id, "timestamp": "2026-01-01T00:00:00Z", "cwd": cwd}, separators=(",", ":")) + "\n"
    + json.dumps({"type": "message", "id": "u", "parentId": None, "timestamp": "2026-01-01T00:00:01Z",
                  "message": {"role": "user", "content": "OMP external prompt"}}, separators=(",", ":")) + "\n")

codex_id = "codex-pres-e2e"
write_text(home / ".codex" / "sessions" / f"rollout-{codex_id}.jsonl",
    json.dumps({"type": "session_meta", "payload": {"id": codex_id, "cwd": cwd}}, separators=(",", ":")) + "\n"
    + json.dumps({"type": "response_item", "payload": {"type": "message", "role": "user",
                  "content": [{"type": "input_text", "text": "Codex external prompt"}]}}, separators=(",", ":")) + "\n")

grok_id = "grok-pres-e2e"
grok_dir = home / ".grok" / "sessions" / "workspace" / grok_id
write_text(grok_dir / "summary.json",
    json.dumps({"info": {"id": grok_id, "cwd": cwd}, "created_at": "2026-01-01T00:00:00Z"}, separators=(",", ":")))
write_text(grok_dir / "chat_history.jsonl",
    json.dumps({"type": "user", "content": "Grok external prompt"}, separators=(",", ":")) + "\n")
PY

    web_spawn_rpi "$root" "$evidence" "$port"
    url="$(web_wait_for_listener "$evidence")"
    log "web.$SCENARIO: listener at $url (token in $root/token)"

    web_run_playwright "$url" "$evidence" "$root/playwright" "$SCRIPT_DIR/presentation_test.mjs" \
        RPI_EVIDENCE="$evidence" || pw_status=$?
    if [ "$pw_status" -eq 0 ]; then
        printf 'playwright: presentation contracts: PASSED\n' | tee "$evidence/playwright-summary.txt"
    elif [ "$pw_status" -eq 1 ]; then
        fail "web.$SCENARIO: playwright SETUP FAILED (node/chromium/npm unavailable)"
    else
        fail "web.$SCENARIO: playwright lane failed (exit $pw_status)"
    fi

    web_sanity_http "$url"
    web_finish_lane "$SCENARIO"
}

main "$@"