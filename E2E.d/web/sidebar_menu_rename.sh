#!/usr/bin/env bash
# Sidebar context-menu rename regression lane — PLAYWRIGHT-ONLY.
#
# Guards the fixed P0 regression: right-click a SessionSidebar row -> Rename,
# then natural (keyboard-event) multi-character typing must accumulate with
# zero per-keystroke select/focus calls, and Enter must commit the exact
# typed name. Exercises the sidebar PORTAL rename input
# (`.session-sidebar__menu-rename`), which the session_ux lane's SUX1.2 does
# not cover (that lane types into the Session panel `#session-rename-input`).
#
# Fixture: one synthetic native saved session ("Sidebar Rename Seed") under an
# isolated HOME; loopback mock provider; real `rpi --listen` serving the
# current-tree web bundle. All evidence labels are synthetic — no session
# paths, names, or tokens in evidence.
#
# Usage: bash E2E.d/web/sidebar_menu_rename.sh [run|list]
set -euo pipefail
SCRIPT_DIR="$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
E2E_DIR="$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd -P)"
# shellcheck source=../lib/common.sh
. "$E2E_DIR/lib/common.sh"
# shellcheck source=lib/fixture.sh
. "$SCRIPT_DIR/lib/fixture.sh"

SCENARIO="web-sidebar-menu-rename"
TOKEN="web-sidebar-rename-e2e-token-$$-$(date +%s)"
SEED_SID="sidebar-rename-seed"

# ---------------------------------------------------------------------------
main() {
    case "${1:-run}" in
        list|--list|--dry-run)
            printf '%s\n' \
              'web-sidebar-menu-rename - P0 repro: sidebar right-click Rename portal input loses all but the last typed character (per-char accumulation assertion + select/focus instrumentation + screenshot evidence) (PLAYWRIGHT-ONLY, hard-fail)'
            return 0
            ;;
        run|all) ;;
        *) fail "usage: $0 [run|list]" ;;
    esac

    require_rpi
    require_cmd node
    require_cmd python3

    local root evidence port url pw_status=0
    prepare_roots
    root="$(scenario_workspace "$SCENARIO")"
    trap 'web_kill_mock "$root"; cleanup_e2e' EXIT HUP INT TERM
    evidence="$EVIDENCE_ROOT/$SCENARIO"
    printf '%s\n' "$TOKEN" >"$root/token"
    printf '{}\n' >"$root/home/.rpi/settings.json"

    # Seed one synthetic native saved session the sidebar will list.
    python3 - "$root/home" "$root/workspace" <<'PY'
import datetime, json, pathlib, sys

home = pathlib.Path(sys.argv[1])
cwd = pathlib.Path(sys.argv[2])
sid = "sidebar-rename-seed"
stamp = datetime.datetime.now(datetime.timezone.utc).isoformat().replace("+00:00", "Z")

encoded = "--" + str(cwd).lstrip("/").replace("/", "-").replace("\\", "-").replace(":", "-") + "--"
d = home / ".rpi" / "sessions" / encoded
d.mkdir(parents=True, exist_ok=True)

def line(record):
    return json.dumps(record, separators=(",", ":")) + "\n"

records = [
    {"type": "session", "version": 3, "id": sid, "timestamp": stamp, "cwd": str(cwd)},
    {"type": "session_info", "id": "si-1", "parentId": None, "timestamp": stamp, "name": "Sidebar Rename Seed"},
    {"type": "message", "id": "m1", "parentId": None, "timestamp": stamp,
     "message": {"role": "user", "content": [{"type": "text", "text": "sidebar rename repro seed turn"}], "timestamp": 1}},
]
(d / f"{stamp[:10]}T000000-{sid}.jsonl").write_text("".join(line(r) for r in records), encoding="utf-8")
PY

    port="$(web_start_mock "$root" "$evidence")"
    web_spawn_rpi "$root" "$evidence" "$port"
    url="$(web_wait_for_listener "$evidence")"
    log "$SCENARIO: listener at $url"

    web_run_playwright "$url" "$evidence" "$root/playwright" "$SCRIPT_DIR/sidebar_menu_rename_test.mjs" \
        || pw_status=$?
    if [ "$pw_status" -ne 0 ]; then
        if [ "$pw_status" -eq 2 ]; then
            # Assertion failure: character loss or per-keystroke re-select.
            web_kill_mock "$root"
            trap cleanup_e2e EXIT HUP INT TERM
            fail "$SCENARIO: regression: sidebar rename lost characters or re-selected per keystroke (exit 2)"
        fi
        fail "$SCENARIO: playwright lane failed (exit $pw_status)"
    fi

    web_sanity_http "$url"
    web_kill_mock "$root"
    trap cleanup_e2e EXIT HUP INT TERM
    web_finish_lane "$SCENARIO"
}

main "$@"