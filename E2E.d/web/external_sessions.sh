#!/usr/bin/env bash
# Web external-session discovery / secure import E2E lane — PLAYWRIGHT-ONLY
# (no agent-browser fallback, no skip).
#
# Seeds deterministic foreign sessions (OMP + Codex + Grok) under the isolated
# fixture HOME layout that SessionCatalog::from_env discovers, starts the real
# `rpi --listen` from a temp workspace, opens `/web` in a real playwright
# browser, and asserts the named behavior matrix in external_sessions_test.mjs:
#   - boot + token connect
#   - Web-only default discovery/provider grouping (rpi/OMP/Codex/Grok)
#   - click a foreign Codex row -> transparent activation of an rpi native
#     import copy (panel session-file under native tree, never the foreign
#     source path)
#   - foreign source file bytes + mtime remain unchanged after import
#   - no duplicate imported/foreign logical row; exactly one import_*.jsonl
#   - select the native copy again and prove lineage/native-copy reuse
#   - explicit sessionImportSources: [] is native-only (foreign rows gone)
#
# The native-only phase rewrites settings.json, kills the listener, and respawns so the
# Web catalog reloads under the authoritative empty import-sources policy.
# Foreign source files stay on disk (read-only contract); they must simply
# stop appearing in the sidebar.
#
# The lane FAILS (non-zero) whenever playwright/chromium cannot be used —
# RPI_WEB_FORCE_PLAYWRIGHT is irrelevant here because there is no fallback.
#
# Usage: bash E2E.d/web/external_sessions.sh [run|list]
set -euo pipefail
SCRIPT_DIR="$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
E2E_DIR="$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd -P)"
# shellcheck source=../lib/common.sh
. "$E2E_DIR/lib/common.sh"
# shellcheck source=lib/fixture.sh
. "$SCRIPT_DIR/lib/fixture.sh"

SCENARIO="web-external_sessions"
TOKEN="web-external_sessions-e2e-token-$$-$(date +%s)"

# ---------------------------------------------------------------------------
main() {
    case "${1:-run}" in
        list|--list|--dry-run)
            printf '%s\n' \
                'web-external-sessions - external sessions: Web default OMP/Codex/Grok discovery+provider grouping (rpi/OMP/Codex/Grok), foreign click imports native copy, foreign bytes/mtime immutable, lineage reuse, no duplicate rows, sessionImportSources:[] native-only (PLAYWRIGHT-ONLY, hard-fail)'
            return 0
            ;;
        run|all) ;;
        *) fail "usage: $0 [run|list]" ;;
    esac

    require_rpi
    # Playwright-only: node is REQUIRED. Missing node is a FAILURE, never a
    # skip and never a fallback to agent-browser.
    require_cmd node
    require_cmd python3
    require_cmd jq

    local root evidence port url pw_status=0 listen_port
    root="$(scenario_workspace "$SCENARIO")"
    git -C "$root/workspace" init -q
    git -C "$root/workspace" config user.email "web-external-sessions@example.com"
    git -C "$root/workspace" config user.name "Web External Sessions"
    git -C "$root/workspace" config commit.gpgsign false
    printf 'web external sessions seed\n' >"$root/workspace/seed.txt"
    git -C "$root/workspace" add -- seed.txt
    git -C "$root/workspace" commit -q -m "web external sessions seed"
    evidence="$EVIDENCE_ROOT/$SCENARIO"
    printf '%s\n' "$TOKEN" >"$root/token"

    # Absent sessionImportSources => Web default NativePi+OMP+Codex+Grok.
    # scenario_workspace already wrote `{}`; keep that explicit for the lane.
    printf '{}\n' >"$root/home/.pi/agent/settings.json"

    # Seed one native row + three foreign sources under the isolated HOME.
    # Paths mirror SessionCatalog::from_env (HOME + PI_CODING_AGENT_DIR) and
    # the catalog test writers (write_omp / write_codex / write_grok).
    python3 - "$root/home" "$root/workspace" "$evidence" <<'PY'
import datetime, json, os, pathlib, sys

home = pathlib.Path(sys.argv[1])
cwd = sys.argv[2]
evidence = pathlib.Path(sys.argv[3])
agent = home / ".pi" / "agent"
sessions_root = agent / "sessions"

def encoded(path: str) -> str:
    s = path
    if s.startswith("/") or s.startswith("\\"):
        s = s[1:]
    return "--" + s.replace("/", "-").replace("\\", "-").replace(":", "-") + "--"

def write_text(path: pathlib.Path, body: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(body, encoding="utf-8")

stamp = datetime.datetime.now(datetime.timezone.utc).isoformat().replace("+00:00", "Z")

# Native seed (always eligible, including the native-only phase).
native_dir = sessions_root / encoded(cwd)
native_dir.mkdir(parents=True, exist_ok=True)
native_sid = "native-ext-seed"
native_file = native_dir / f"{stamp[:10]}T000000-{native_sid}.jsonl"
native_records = [
    {"type": "session", "version": 3, "id": native_sid, "timestamp": stamp, "cwd": cwd},
    # Explicit session_info name: the Web sidebar `temporary` marker must
    # never apply to this legal native seed (native + unnamed + <10KiB + cwd
    # under the OS temp root would otherwise hide it by default).
    {
        "type": "session_info",
        "id": "si-1",
        "parentId": None,
        "timestamp": stamp,
        "name": native_sid,
    },
    {
        "type": "message",
        "id": "m1",
        "parentId": None,
        "timestamp": stamp,
        "message": {
            "role": "user",
            "content": [{"type": "text", "text": "native-ext-seed"}],
            "timestamp": 1,
        },
    },
]
native_file.write_text(
    "".join(json.dumps(r, separators=(",", ":")) + "\n" for r in native_records),
    encoding="utf-8",
)

# OMP (pi-compatible v3 under ~/.omp/agent/sessions/--work--/).
omp_id = "omp-ext-e2e"
omp_file = home / ".omp" / "agent" / "sessions" / "--work--" / f"{omp_id}.jsonl"
write_text(
    omp_file,
    (
        json.dumps(
            {
                "type": "session",
                "version": 3,
                "id": omp_id,
                "timestamp": "2026-01-01T00:00:00Z",
                "cwd": cwd,
            },
            separators=(",", ":"),
        )
        + "\n"
        + json.dumps(
            {
                "type": "message",
                "id": "u",
                "parentId": None,
                "timestamp": "2026-01-01T00:00:01Z",
                "message": {"role": "user", "content": "OMP external prompt"},
            },
            separators=(",", ":"),
        )
        + "\n"
    ),
)

# Codex rollout under ~/.codex/sessions/rollout-<id>.jsonl.
codex_id = "codex-ext-e2e"
codex_file = home / ".codex" / "sessions" / f"rollout-{codex_id}.jsonl"
write_text(
    codex_file,
    (
        json.dumps(
            {
                "type": "session_meta",
                "payload": {"id": codex_id, "cwd": cwd},
            },
            separators=(",", ":"),
        )
        + "\n"
        + json.dumps(
            {
                "type": "response_item",
                "payload": {
                    "type": "message",
                    "role": "user",
                    "content": [{"type": "input_text", "text": "Codex external prompt"}],
                },
            },
            separators=(",", ":"),
        )
        + "\n"
        + json.dumps(
            {
                "type": "response_item",
                "payload": {
                    "type": "message",
                    "role": "assistant",
                    "content": [{"type": "output_text", "text": "Codex external reply"}],
                },
            },
            separators=(",", ":"),
        )
        + "\n"
    ),
)

# Grok summary.json + chat_history.jsonl under ~/.grok/sessions/<cwd>/<id>/.
grok_id = "grok-ext-e2e"
grok_dir = home / ".grok" / "sessions" / "workspace" / grok_id
write_text(
    grok_dir / "summary.json",
    json.dumps(
        {
            "info": {"id": grok_id, "cwd": cwd},
            "created_at": "2026-01-01T00:00:00Z",
        },
        separators=(",", ":"),
    )
    + "\n",
)
write_text(
    grok_dir / "chat_history.jsonl",
    (
        json.dumps(
            {
                "role": "user",
                "content": "Grok external prompt",
                "timestamp": "2026-01-01T00:00:01Z",
            },
            separators=(",", ":"),
        )
        + "\n"
        + json.dumps(
            {
                "role": "assistant",
                "content": "Grok external reply",
                "timestamp": "2026-01-01T00:00:02Z",
            },
            separators=(",", ":"),
        )
        + "\n"
    ),
)

# Pin mtimes deterministically so boot restore prefers the native seed when
# the catalog is sorted newest-first (native newest).
base = native_file.stat().st_mtime
os.utime(native_file, (base + 3, base + 3))
os.utime(omp_file, (base + 2, base + 2))
os.utime(codex_file, (base + 1, base + 1))
os.utime(grok_dir / "summary.json", (base, base))

meta = {
    "cwd": cwd,
    "nativeSessionDir": str(native_dir),
    "nativeSessionsRoot": str(sessions_root),
    "nativeId": native_sid,
    "nativePath": str(native_file),
    "ompId": omp_id,
    "ompPath": str(omp_file),
    "codexId": codex_id,
    "codexPath": str(codex_file),
    "grokId": grok_id,
    "grokPath": str(grok_dir / "summary.json"),
    "grokChatPath": str(grok_dir / "chat_history.jsonl"),
}
(evidence / "seed-meta.json").write_text(json.dumps(meta, indent=2), encoding="utf-8")
print(json.dumps(meta))
PY

    # Standalone runs use the checked-in bundle; coverage runs inherit the
    # instrumented RPI_WEB_DEV_DIR so their V8 payload remains source-mappable.
    if [ -z "${RPI_COVERAGE_DIR:-}" ]; then
        mkdir -p "$root/web-dist"
        cp "$REPO_ROOT/crates/pi-cli/web/dist/index.html" "$root/web-dist/index.html"
        export RPI_WEB_DEV_DIR="$root/web-dist"
    fi

    port="$(MOCK_SCENARIO=sessions web_start_mock "$root" "$evidence")"
    web_spawn_rpi "$root" "$evidence" "$port"
    url="$(web_wait_for_listener "$evidence")"
    log "$SCENARIO: listener at $url (default Web import sources)"

    # Capture the fixed listen port from the first banner so the native-only
    # respawn can keep the same /web URL for the second playwright phase.
    listen_port="$(printf '%s\n' "$url" | sed -n 's#.*://[^:]*:\([0-9]*\)/web#\1#p')"
    [ -n "$listen_port" ] || fail "$SCENARIO: could not parse listen port from $url"

    # Phase 1: default Web discovery + import/activation contracts.
    web_run_playwright "$url" "$evidence" "$root/playwright" "$SCRIPT_DIR/external_sessions_test.mjs" \
        "RPI_PHASE=default" \
        "RPI_SEED_META=$evidence/seed-meta.json" \
        "RPI_NATIVE_SESSIONS_ROOT=$root/home/.pi/agent/sessions" \
        || pw_status=$?
    if [ "$pw_status" -ne 0 ]; then
        fail "$SCENARIO: playwright default phase failed (exit $pw_status) — external-sessions lane is playwright-only"
    fi

    # Phase 2: explicit empty sessionImportSources is native-only. Rewrite
    # settings, kill the listener, respawn on the same port (foreign files
    # remain on disk, read-only).
    printf '%s\n' '{"sessionImportSources":[]}' >"$root/home/.pi/agent/settings.json"
    web_kill_rpi
    sleep 0.3
    web_spawn_rpi "$root" "$evidence" "$port" "$listen_port" "2"
    url="$(web_wait_for_listener "$evidence" "2")"
    log "$SCENARIO: listener respawned at $url (sessionImportSources:[])"

    pw_status=0
    web_run_playwright "$url" "$evidence" "$root/playwright-native" "$SCRIPT_DIR/external_sessions_test.mjs" \
        "RPI_PHASE=native_only" \
        "RPI_SEED_META=$evidence/seed-meta.json" \
        "RPI_NATIVE_SESSIONS_ROOT=$root/home/.pi/agent/sessions" \
        || pw_status=$?
    if [ "$pw_status" -ne 0 ]; then
        fail "$SCENARIO: playwright native-only phase failed (exit $pw_status) — external-sessions lane is playwright-only"
    fi

    # Merge executed-assertion evidence from both phases into one matrix file.
    python3 - "$evidence" <<'PY'
import json, pathlib, sys
evidence = pathlib.Path(sys.argv[1])
executed = []
seen = set()
for name in ("coverage-assertions-default.json", "coverage-assertions-native_only.json"):
    path = evidence / name
    if not path.is_file():
        raise SystemExit(f"missing phase evidence: {path}")
    payload = json.loads(path.read_text(encoding="utf-8"))
    for item in payload.get("executed") or []:
        if item not in seen:
            seen.add(item)
            executed.append(item)
(evidence / "coverage-assertions.json").write_text(
    json.dumps({"executed": executed}, indent=2) + "\n",
    encoding="utf-8",
)
print(f"merged {len(executed)} assertion ids")
PY

    web_sanity_http "$url"
    web_finish_lane "$SCENARIO"
}

main "$@"
