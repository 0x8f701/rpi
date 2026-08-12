#!/usr/bin/env bash
# Web all-project session catalog + cross-project New-session storage E2E
# lane — PLAYWRIGHT-ONLY (no agent-browser fallback, no skip).
#
# Seeds two valid native sessions (version-3 JSONL, header + user message)
# for TWO temp project cwds under the SAME isolated profile native tree
# (`<agent>/sessions/--<encoded-cwd>--/`), starts the real `rpi --listen`
# from project A's cwd, opens `/web` in a real playwright browser, and
# asserts the all-project catalog contract (see projects_test.mjs for the
# P0.x-P4.x assertion matrix):
#   P0  boot: the newest catalog row (project A) is restored as the active
#       session; the sidebar groups BOTH projects under the rpi provider
#   P1  Session panel: backend cwd/project fields = project A
#   P2  switching to the project-B row activates backend cwd B (panel
#       project/cwd/session-file fields all flip to B)
#   P3  New session inherits B's cwd and its file lives under B's ENCODED
#       default session directory (browser-visible path + sidebar row)
#   P4  on-disk proof: exactly one new session file under B's encoded dir
#       (header cwd == B) and NO new file under A's dir
#
# The lane FAILS (non-zero) whenever playwright/chromium cannot be used —
# RPI_WEB_FORCE_PLAYWRIGHT is irrelevant here because there is no fallback.
#
# Usage: bash E2E.d/web/projects.sh [run|list]
set -euo pipefail
SCRIPT_DIR="$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
E2E_DIR="$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd -P)"
# shellcheck source=../lib/common.sh
. "$E2E_DIR/lib/common.sh"
# shellcheck source=lib/fixture.sh
. "$SCRIPT_DIR/lib/fixture.sh"

SCENARIO="web-projects"
TOKEN="web-projects-e2e-token-$$-$(date +%s)"

# ---------------------------------------------------------------------------
main() {
    case "${1:-run}" in
        list|--list|--dry-run)
            printf '%s\n' \
                'web-projects - all-project native session catalog + cross-project New-session storage: seeded A/B sessions listed as two sidebar project subgroups under the rpi provider, project-B switch activates backend cwd B, New session persists under B encoded default dir, on-disk proof (PLAYWRIGHT-ONLY, hard-fail)'
            return 0
            ;;
        run|all) ;;
        *) fail "usage: $0 [run|list]" ;;
    esac

    require_rpi
    # Playwright-only: node is REQUIRED. Missing node is a FAILURE, never a
    # skip and never a fallback to agent-browser.
    require_cmd node

    local root evidence port url pw_status=0
    root="$(scenario_workspace "$SCENARIO")"
    # Project A is the listener's cwd: the fixture workspace. Project B is a
    # SIBLING temp project directory — a second project under the same
    # isolated profile, never a child of A (the two encoded default session
    # directories must be distinct roots of the native tree).
    local project_b="$root/project-b"
    mkdir -p "$project_b"
    git -C "$root/workspace" init -q
    git -C "$root/workspace" config user.email "web-projects@example.com"
    git -C "$root/workspace" config user.name "Web Projects"
    git -C "$root/workspace" config commit.gpgsign false
    printf 'web projects seed\n' >"$root/workspace/seed.txt"
    git -C "$root/workspace" add -- seed.txt
    git -C "$root/workspace" commit -q -m "web projects seed"
    evidence="$EVIDENCE_ROOT/$SCENARIO"
    printf '%s\n' "$TOKEN" >"$root/token"

    # Seed two valid native sessions under the isolated profile's native
    # sessions tree, mirroring `pi_coding::start_session_in` output exactly:
    # `<agent>/sessions/--<encoded-cwd>--/<stamp>T000000-<id>.jsonl` with a
    # version-3 session header (cwd recorded) + one user message (the catalog
    # summary / sidebar title). Project B is seeded first and project A's
    # mtime is pinned one second NEWER so the Web boot restore (first catalog
    # row = newest) deterministically lands on project A. The resolved
    # session directories are written to $evidence/seed-dirs.json for the
    # playwright env (single source of truth for the encoding).
    python3 - "$root/home/.pi/agent/sessions" "$root/workspace" "$project_b" "$evidence" <<'PY'
import datetime, json, os, pathlib, sys
sessions_root = pathlib.Path(sys.argv[1])
cwd_a = sys.argv[2]
cwd_b = sys.argv[3]
evidence = pathlib.Path(sys.argv[4])

def encoded(cwd: str) -> str:
    s = cwd
    if s.startswith('/') or s.startswith('\\'):
        s = s[1:]
    return '--' + s.replace('/', '-').replace('\\', '-').replace(':', '-') + '--'

def write_seed(cwd: str, sid: str, summary: str, stamp: str) -> pathlib.Path:
    directory = sessions_root / encoded(cwd)
    directory.mkdir(parents=True, exist_ok=True)
    records = [
        {'type': 'session', 'version': 3, 'id': sid, 'timestamp': stamp, 'cwd': cwd},
        # Explicit session_info name: the Web sidebar's `temporary` marker
        # (native + unnamed + <10KiB + cwd under the OS temp root) must never
        # apply to legal fixture sessions — the lane asserts both seeded rows
        # are listed, and a named row is never temporary.
        {'type': 'session_info', 'id': 'si-1', 'parentId': None, 'timestamp': stamp,
         'name': summary},
        {'type': 'message', 'id': 'm1', 'parentId': None, 'timestamp': stamp,
         'message': {'role': 'user', 'content': [{'type': 'text', 'text': summary}], 'timestamp': 1}},
    ]
    target = directory / f'{stamp[:10]}T000000-{sid}.jsonl'
    target.write_text(''.join(json.dumps(r, separators=(',', ':')) + '\n' for r in records), encoding='utf-8')
    return target

stamp = datetime.datetime.now(datetime.timezone.utc).isoformat().replace('+00:00', 'Z')
file_b = write_seed(cwd_b, 'project-b-seed', 'project-b-seed', stamp)
file_a = write_seed(cwd_a, 'project-a-seed', 'project-a-seed', stamp)
# Pin mtimes deterministically: B at T, A at T+1s (A = newest catalog row).
base = file_b.stat().st_mtime
os.utime(file_b, (base, base))
os.utime(file_a, (base + 1, base + 1))
(evidence / 'seed-dirs.json').write_text(json.dumps({
    'sessionDirA': str(sessions_root / encoded(cwd_a)),
    'sessionDirB': str(sessions_root / encoded(cwd_b)),
    'cwdA': cwd_a,
    'cwdB': cwd_b,
}, indent=2), encoding='utf-8')
PY

    local session_dir_a session_dir_b
    session_dir_a="$(jq -r '.sessionDirA' "$evidence/seed-dirs.json")"
    session_dir_b="$(jq -r '.sessionDirB' "$evidence/seed-dirs.json")"

    # Standalone runs serve the checked-in Web bundle from a fixture-local
    # copy, so they exercise the frontend the repo ships even if the binary
    # predates the current dist. Coverage runs instead inherit the
    # instrumented RPI_WEB_DEV_DIR exported by coverage.sh; overriding it here
    # would load a bundle without the inline source map and invalidate that
    # lane's V8 payload.
    if [ -z "${RPI_COVERAGE_DIR:-}" ]; then
        mkdir -p "$root/web-dist"
        cp "$REPO_ROOT/crates/pi-cli/web/dist/index.html" "$root/web-dist/index.html"
        export RPI_WEB_DEV_DIR="$root/web-dist"
    fi

    port="$(MOCK_SCENARIO=sessions web_start_mock "$root" "$evidence")"
    web_spawn_rpi "$root" "$evidence" "$port"
    url="$(web_wait_for_listener "$evidence")"
    log "$SCENARIO: listener at $url"

    # Hard playwright run: exit 1 from web_run_playwright means the npm
    # install failed — for THIS lane that is a real failure (no fallback).
    web_run_playwright "$url" "$evidence" "$root/playwright" "$SCRIPT_DIR/projects_test.mjs" \
        "RPI_PROJECT_A=$root/workspace" \
        "RPI_PROJECT_B=$project_b" \
        "RPI_SESSION_DIR_A=$session_dir_a" \
        "RPI_SESSION_DIR_B=$session_dir_b" \
        || pw_status=$?
    if [ "$pw_status" -ne 0 ]; then
        fail "$SCENARIO: playwright lane failed (exit $pw_status) — projects lane is playwright-only"
    fi

    web_sanity_http "$url"
    web_finish_lane "$SCENARIO"
}

main "$@"
