#!/usr/bin/env bash
# Web session UX E2E lane — PLAYWRIGHT-ONLY (no agent-browser fallback, no
# skip). Isolated lane: owns its fixture, shares only the lib helpers.
#
# Spawns the real `rpi --listen` binary with a token file and the loopback
# mock provider, resumes a SEEDED native saved session as the listener
# primary (--resume), opens `/web` in a real playwright browser, and asserts
# the matrix in session_ux_test.mjs:
#   SUX0  boot lists seeded native + foreign rows; native row context menu
#         exposes the full native idle item set (Copy context first)
#   SUX1  Session panel keyboard rename: the rename input accumulates a
#         multi-character name PER CHARACTER (keyboard events, never fill),
#         Enter immediately sends exactly one `set_session_name` frame with
#         the exact typed name and no sessionPath, and the panel Name and
#         sidebar row title update while the header renders no duplicate
#         session label (#session-name is permanently absent)
#   SUX2  right-click Copy context on a native saved row: exactly one
#         `session_copy_context` frame carrying the captured row path, the
#         clipboard starts with the fixed format headers, stays under the
#         64KiB bound, contains the seeded (sanitized) conversation, goal,
#         and todo, never switches the active session, and toasts exactly
#         "Copied context"
#   SUX3  truncation fixture: an oversized conversation copies as
#         "Truncated: yes" with the section marker and the truncated toast
#   SUX4  foreign rows never expose Copy context (Export HTML only) and the
#         lane ends with zero page/console errors
#
# The lane FAILS (non-zero) whenever playwright/chromium cannot be used.
#
# Usage: bash E2E.d/web/session_ux.sh [run|list]
set -euo pipefail
SCRIPT_DIR="$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
E2E_DIR="$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd -P)"
# shellcheck source=../lib/common.sh
. "$E2E_DIR/lib/common.sh"
# shellcheck source=lib/fixture.sh
. "$SCRIPT_DIR/lib/fixture.sh"

SCENARIO="web-session_ux"
TOKEN="web-session-ux-e2e-token-$$-$(date +%s)"
ACTIVE_SEED_ID="session-ux-rename-seed"

# ---------------------------------------------------------------------------
main() {
    case "${1:-run}" in
        list|--list|--dry-run)
            printf '%s\n' \
                'web-session-ux - Web Session panel keyboard rename (per-char input accumulation, exact set_session_name frame/name, panel/header/sidebar update) + right-click Copy context on native saved rows (session_copy_context frame for captured row path, fixed format headers, bounded sanitized conversation/goal/todo, truncation fixture, no active-session switch, exact Copied context toasts, foreign rows export-only, zero browser errors) (PLAYWRIGHT-ONLY, hard-fail)'
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

    local root evidence port url pw_status=0
    prepare_roots
    root="$(scenario_workspace "$SCENARIO")"
    # The mock provider is started via command substitution below, so its
    # register_pid call is lost in the subshell (web_start_mock persists the
    # PID to $root/mock.pid instead). Re-trap so every failure path (fail,
    # playwright exit, signal) still reaps the mock via web_kill_mock before
    # cleanup_e2e removes the workspace; cleanup_e2e keeps reaping the
    # parent-registered listener PID.
    trap 'web_kill_mock "$root"; cleanup_e2e' EXIT HUP INT TERM
    git -C "$root/workspace" init -q
    git -C "$root/workspace" config user.email "web-session-ux@example.com"
    git -C "$root/workspace" config user.name "Web Session UX"
    git -C "$root/workspace" config commit.gpgsign false
    printf 'web session ux seed\n' >"$root/workspace/seed.txt"
    git -C "$root/workspace" add -- seed.txt
    git -C "$root/workspace" commit -q -m "web session ux seed"
    evidence="$EVIDENCE_ROOT/$SCENARIO"
    printf '%s\n' "$TOKEN" >"$root/token"

    # Default Web discovery ({} = rpi + Pi import sources): the foreign Pi row
    # must be listed while every mutation stays native-only.
    printf '{}\n' >"$root/home/.rpi/settings.json"

    # Seed the deterministic fixture under the isolated HOME layout that
    # SessionCatalog::from_env discovers: three native saved sessions (rename
    # target resumed as the listener primary, goal/todo/conversation context
    # seed, oversized truncation seed) plus one foreign Pi session.
    python3 - "$root/home" "$root/workspace" "$evidence" <<'PY'
import datetime, json, os, pathlib, sys

home = pathlib.Path(sys.argv[1])
cwd = sys.argv[2]
evidence = pathlib.Path(sys.argv[3])
sessions_root = home / ".rpi" / "sessions"

def encoded(path: str) -> str:
    s = path
    if s.startswith("/") or s.startswith("\\"):
        s = s[1:]
    return "--" + s.replace("/", "-").replace("\\", "-").replace(":", "-") + "--"

def line(record) -> str:
    return json.dumps(record, separators=(",", ":")) + "\n"

def session_header(sid: str, stamp: str, cwd: str) -> dict:
    return {"type": "session", "version": 3, "id": sid, "timestamp": stamp, "cwd": cwd}

def session_info(sid: str, stamp: str, name: str) -> dict:
    return {"type": "session_info", "id": "si-1", "parentId": None, "timestamp": stamp, "name": name}

def message(mid: str, parent, stamp: str, role: str, text: str, ts: int) -> dict:
    return {
        "type": "message",
        "id": mid,
        "parentId": parent,
        "timestamp": stamp,
        "message": ({"role": role, "content": [{"type": "text", "text": text}], "api": "openai-completions", "provider": "user-steering", "model": "mock", "stopReason": "stop", "timestamp": ts} if role == "assistant" else {"role": role, "content": [{"type": "text", "text": text}], "timestamp": ts}),
    }

stamp = datetime.datetime.now(datetime.timezone.utc).isoformat().replace("+00:00", "Z")

# Fake credential-shaped strings are assembled from parts so no literal
# credential shape ever appears in source or reports; the Copy context
# sanitizer must redact them out of the clipboard.
fake_sk = "sk-" + "fakekey0123456789abcdef"
fake_api_value = "fake-cred-value-123456"

native_dir = sessions_root / encoded(cwd)
native_dir.mkdir(parents=True, exist_ok=True)

def write_native(sid: str, name: str, records) -> pathlib.Path:
    path = native_dir / f"{stamp[:10]}T000000-{sid}.jsonl"
    path.write_text("".join(line(r) for r in records), encoding="utf-8")
    return path

# 1) Rename seed: resumed as the listener primary, so the ACTIVE session is a
#    native saved session and the Session panel rename exercises it.
active_sid = "session-ux-rename-seed"
active_records = [
    session_header(active_sid, stamp, cwd),
    session_info(active_sid, stamp, "Rename Seed Alpha"),
    message("m1", None, stamp, "user", "rename seed user turn", 1),
]
active_path = write_native(active_sid, "Rename Seed Alpha", active_records)

# 2) Context seed: named native session with a bounded conversation, an active
#    goal journal event, and a todo snapshot for the Copy context assertions.
context_sid = "session-ux-context-seed"
goal_ts = "2026-08-30T12:00:00Z"
context_records = [
    session_header(context_sid, stamp, cwd),
    session_info(context_sid, stamp, "Context Seed Beta"),
    message(
        "u1", None, stamp, "user",
        "session-ux-context user turn marker: inspect " + cwd + "/seed.txt then ship. "
        + "api_key=" + fake_api_value + " " + fake_sk,
        1,
    ),
    message("a1", "u1", stamp, "assistant", "session-ux-context assistant turn marker: plan committed", 2),
    {
        "type": "custom",
        "id": "g1",
        "parentId": "a1",
        "timestamp": stamp,
        "customType": "pi.goal.event",
        "data": {
            "version": 1,
            "event": {
                "revision": 1,
                "timestamp": goal_ts,
                "kind": {"type": "created"},
                "goal": {
                    "id": "goal-session-ux-fixture",
                    "objective": "Ship the session UX copy context fixture",
                    "lifecycle": "active",
                    "createdAt": goal_ts,
                    "updatedAt": goal_ts,
                    "usage": {"tokensUsed": 0, "activeTimeSeconds": 0},
                },
            },
        },
    },
    {
        "type": "todo_snapshot",
        "id": "todo-1",
        "parentId": "g1",
        "timestamp": stamp,
        "state": {
            "phases": [
                {
                    "name": "Build",
                    "tasks": [
                        {"id": "t1", "content": "compile the context fixture", "status": "in_progress",
                         "dependsOn": [], "ready": True, "waitingOn": []},
                        {"id": "t2", "content": "verify the context fixture", "status": "pending",
                         "dependsOn": [], "ready": True, "waitingOn": []},
                    ],
                }
            ],
            "storage": "session",
        },
    },
]
context_path = write_native(context_sid, "Context Seed Beta", context_records)

# 3) Truncation seed: one ~73KiB user turn so the copy-context conversation
#    section is bounded by the 64KiB artifact ceiling (marker + Truncated: yes).
truncate_sid = "session-ux-truncate-seed"
huge_text = "session-ux-truncate marker. " + "filler " * 10000
truncate_records = [
    session_header(truncate_sid, stamp, cwd),
    session_info(truncate_sid, stamp, "Truncate Seed Gamma"),
    message("m1", None, stamp, "user", huge_text, 1),
]
truncate_path = write_native(truncate_sid, "Truncate Seed Gamma", truncate_records)

# 4) Foreign Pi session under ~/.pi/agent/sessions (immutable foreign source).
pi_sid = "pi-session-ux-foreign"
pi_dir = home / ".pi" / "agent" / "sessions" / "--work--"
pi_dir.mkdir(parents=True, exist_ok=True)
pi_path = pi_dir / f"{pi_sid}.jsonl"
pi_records = [
    session_header(pi_sid, stamp, cwd),
    message("pu1", None, stamp, "user", "Pi foreign prompt", 1),
    message("pa1", "pu1", stamp, "assistant", "Pi foreign reply", 2),
]
pi_path.write_text("".join(line(r) for r in pi_records), encoding="utf-8")

# Pin mtimes: rename seed newest (resume target), then context, truncate, and
# the foreign Pi source oldest.
base = active_path.stat().st_mtime
os.utime(active_path, (base + 3, base + 3))
os.utime(context_path, (base + 2, base + 2))
os.utime(truncate_path, (base + 1, base + 1))
os.utime(pi_path, (base, base))

meta = {
    "cwd": cwd,
    "activeId": active_sid,
    "activePath": str(active_path),
    "contextId": context_sid,
    "contextPath": str(context_path),
    "truncateId": truncate_sid,
    "truncatePath": str(truncate_path),
    "foreignId": pi_sid,
    "foreignPath": str(pi_path),
}
(evidence / "seed-meta.json").write_text(json.dumps(meta, indent=2), encoding="utf-8")
print(json.dumps({k: v for k, v in meta.items() if not k.endswith("Path")}))
PY

    port="$(web_start_mock "$root" "$evidence")"
    # The listener primary IS the seeded native session: the Session panel
    # rename below exercises a real native saved session end to end.
    WEB_SPAWN_RESUME="$ACTIVE_SEED_ID" web_spawn_rpi "$root" "$evidence" "$port"
    url="$(web_wait_for_listener "$evidence")"
    log "$SCENARIO: listener at $url (primary = $ACTIVE_SEED_ID)"

    web_run_playwright "$url" "$evidence" "$root/playwright" "$SCRIPT_DIR/session_ux_test.mjs" \
        "RPI_SEED_META=$evidence/seed-meta.json" \
        || pw_status=$?
    if [ "$pw_status" -ne 0 ]; then
        fail "$SCENARIO: playwright lane failed (exit $pw_status) — session-ux lane is playwright-only"
    fi

    web_sanity_http "$url"
    # Explicit success-path reap: the mock outlives the listener otherwise
    # (command substitution lost the parent PID registration). Idempotent —
    # the EXIT trap above is a no-op once mock.pid is consumed.
    web_kill_mock "$root"
    # Success path: the mock is dead, so the failure trap's `web_kill_mock
    # "$root"` reference must not survive past main's return — under set -u
    # the local root is gone by the time the EXIT trap fires. Re-arm to
    # cleanup_e2e only, mirroring chat.sh.
    trap cleanup_e2e EXIT HUP INT TERM
    web_finish_lane "$SCENARIO"
}

main "$@"