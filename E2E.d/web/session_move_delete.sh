#!/usr/bin/env bash
# Session management Move/Delete browser lane — PLAYWRIGHT-ONLY.
#
# Guards the shared product contract for the SessionSidebar Move/Delete menu
# actions against a real `rpi --listen` with the content-routed `sessions`
# mock scenario:
#   - Action eligibility replaces the old shared loaded guard: the ACTIVE
#     row disables Delete/Move (explicit switch-away guidance); a non-active
#     row is eligible whether loaded (idle secondary) or unloaded.
#   - A BUSY loaded secondary (held mock stream) is refused by the backend
#     with exactly one bounded error toast and ZERO catalog mutation.
#   - An idle loaded row deletes with exactly one session_delete, row
#     removal, and one Undo restore (session_restore) inside the 5s window.
#   - Move relocates a saved session under another project directory (NOT a
#     runtime cwd change): the empty picker is a disabled submit, the
#     explicit absolute target sends exactly one session_move, the response
#     destination row is validated by the client, the catalog refreshes
#     once, the active session is retained, and the destination row appears
#     under the target project group; an explicit click then restores the
#     moved session's transcript.
#   - Foreign rows stay export-only. Desktop and mobile (drawer + touch
#     long-press) phases both assert eligibility.
#
# Fixture: isolated HOME; four seeded rows —
#   smd-active-a   native, resumed as the listener PRIMARY (active)
#   smd-idle-b     native, same project (loaded by the test when needed)
#   smd-unloaded-c native, same project (unloaded; the move subject)
#   codex-ext-smd  foreign Codex rollout (export-only)
# plus a pre-created absolute move target directory ($root/projects/beta).
# All evidence labels are synthetic — no session paths, names, or tokens.
#
# Usage: bash E2E.d/web/session_move_delete.sh [run|list]
set -euo pipefail
SCRIPT_DIR="$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
E2E_DIR="$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd -P)"
# shellcheck source=../lib/common.sh
. "$E2E_DIR/lib/common.sh"
# shellcheck source=lib/fixture.sh
. "$SCRIPT_DIR/lib/fixture.sh"

SCENARIO="web-session-move-delete"
TOKEN="web-session-move-delete-e2e-token-$$-$(date +%s)"
MOCK_SCENARIO=sessions

# ---------------------------------------------------------------------------
main() {
    case "${1:-run}" in
        list|--list|--dry-run)
            printf '%s\n' \
              'web-session-move-delete - SessionSidebar Move/Delete eligibility: active disabled, non-active loaded/unloaded enabled, busy refusal one toast no mutation, idle delete+undo, move to another project with grouping/active retention, foreign export-only, desktop+mobile (PLAYWRIGHT-ONLY, hard-fail)'
            return 0
            ;;
        run|all) ;;
        *) fail "usage: $0 [run|list]" ;;
    esac

    require_rpi
    require_cmd node
    require_cmd python3

    local root evidence port url mock_port pw_status=0
    prepare_roots
    root="$(scenario_workspace "$SCENARIO")"
    trap 'web_kill_mock "$root"; cleanup_e2e' EXIT HUP INT TERM
    evidence="$EVIDENCE_ROOT/$SCENARIO"
    printf '%s\n' "$TOKEN" >"$root/token"
    printf '{}\n' >"$root/home/.rpi/settings.json"

    # Pre-create the explicit absolute move target project directory.
    mkdir -p "$root/projects/beta"

    # Seed the three native rows under the listener cwd (one provider/project
    # group) plus the foreign Codex rollout under ~/.codex.
    python3 - "$root/home" "$root/workspace" <<'PY'
import datetime, json, pathlib, sys

home = pathlib.Path(sys.argv[1])
cwd = pathlib.Path(sys.argv[2])
stamp = datetime.datetime.now(datetime.timezone.utc).isoformat().replace("+00:00", "Z")
day = stamp[:10]

encoded = "--" + str(cwd).lstrip("/").replace("/", "-").replace("\\", "-").replace(":", "-") + "--"
d = home / ".rpi" / "sessions" / encoded
d.mkdir(parents=True, exist_ok=True)

def line(record):
    return json.dumps(record, separators=(",", ":")) + "\n"

def seed_native(sid, name, user_text, reply_text):
    records = [
        {"type": "session", "version": 3, "id": sid, "timestamp": stamp, "cwd": str(cwd)},
        {"type": "session_info", "id": f"si-{sid}", "parentId": None, "timestamp": stamp, "name": name},
        {"type": "message", "id": f"m1-{sid}", "parentId": None, "timestamp": stamp,
         "message": {"role": "user", "content": [{"type": "text", "text": user_text}], "timestamp": 1}},
        {"type": "message", "id": f"m2-{sid}", "parentId": f"m1-{sid}", "timestamp": stamp,
         "message": {"role": "assistant", "content": [{"type": "text", "text": reply_text}], "api": "openai-completions", "provider": "user-steering", "model": "mock", "stopReason": "stop", "timestamp": 2}},
    ]
    (d / f"{day}T000000-{sid}.jsonl").write_text("".join(line(r) for r in records), encoding="utf-8")

# Exact seed invocations: A is resumed by the listener as the active PRIMARY
# (WEB_SPAWN_RESUME below); B starts unloaded and is loaded by the test; C
# stays unloaded and is the move subject. All three share the listener cwd,
# so the sidebar groups them under one provider/project group.
seed_native("smd-active-a", "SMD Active A", "smd active a seed turn", "smd-a-seed-reply")
seed_native("smd-idle-b", "SMD Idle B", "smd idle b seed turn", "smd-b-seed-reply")
seed_native("smd-unloaded-c", "SMD Unloaded C", "smd unloaded c seed turn", "smd-c-seed-reply")

# Foreign Codex source: export-only, never Delete/Move.
codex_dir = home / ".codex" / "sessions"
codex_dir.mkdir(parents=True, exist_ok=True)
codex_id = "codex-ext-smd"
codex_file = codex_dir / f"rollout-{codex_id}.jsonl"
codex_file.write_text(
    json.dumps({"type": "session_meta", "payload": {"id": codex_id, "cwd": str(cwd)}}, separators=(",", ":")) + "\n"
    + json.dumps({"type": "response_item", "payload": {"type": "message", "role": "user",
        "content": [{"type": "input_text", "text": "smd codex foreign turn"}]}}, separators=(",", ":")) + "\n"
    + json.dumps({"type": "response_item", "payload": {"type": "message", "role": "assistant",
        "content": [{"type": "output_text", "text": "smd-codex-seed-reply"}]}}, separators=(",", ":")) + "\n",
    encoding="utf-8",
)
PY

    port="$(web_start_mock "$root" "$evidence")"
    mock_port="$(cat "$root/mock-port.txt")"
    # Resume the seeded A as the authoritative active primary.
    WEB_SPAWN_RESUME=smd-active-a web_spawn_rpi "$root" "$evidence" "$port"
    url="$(web_wait_for_listener "$evidence")"
    log "$SCENARIO: listener at $url"

    web_run_playwright "$url" "$evidence" "$root/playwright" "$SCRIPT_DIR/session_move_delete_test.mjs" \
        "MOCK_PORT=$mock_port" \
        "SMD_TARGET_CWD=$root/projects/beta" \
        "SMD_SOURCE_PROJECT=workspace" \
        "SMD_TARGET_PROJECT=beta" \
        || pw_status=$?
    if [ "$pw_status" -ne 0 ]; then
        fail "$SCENARIO: playwright lane failed (exit $pw_status)"
    fi

    web_sanity_http "$url"
    web_kill_mock "$root"
    trap cleanup_e2e EXIT HUP INT TERM
    web_finish_lane "$SCENARIO"
}

main "$@"