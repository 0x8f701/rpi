#!/usr/bin/env bash
# Web Files-drawer directory-toggle latency lane — PLAYWRIGHT-ONLY hard gate.
#
# Spawns the real `rpi --listen` binary (loopback mock provider + token file)
# from a fixture workspace holding a git repo with 500 dirty files across a
# deep directory tree, and a seeded native session carrying 1500 transcript
# messages (the large parent), then opens /web in a real browser and measures
# the directory-toggle hot path end to end:
#   - REAL pointer click on a tree directory row is captured in-page. The
#     synchronous store write happens first in the click handler, followed by
#     the one file_explorer_list RPC send; a MutationObserver timestamps the
#     aria-expanded/child-row COMMIT and requestAnimationFrame timestamps the
#     next paint. The commit must land by that first animation frame and before
#     the exact successful listing response (the ticket's stated budget — no
#     arbitrary duration threshold; observed durations are printed and
#     published inside the run's immutable evidence run directory, referenced
#     by the atomically-renamed $RPI_EVIDENCE/current.json manifest/pointer).
#   - The commit is independent of the listing response: exactly ONE
#     file_explorer_list RPC for the toggled path on first expansion, and the
#     exact correlated response arrives after the commit (recorded via a
#     record-only WebSocket wrapper).
#   - PARENT RENDER ISOLATION: a window.__REACT_DEVTOOLS_GLOBAL_HOOK__
#     onCommitFiberRoot probe (installed before the bundle runs) finds the
#     fiber whose props carry the >=1200-entry transcript array and counts
#     commits that change its props object identity. During a directory
#     toggle the count is ZERO — the 1500-message transcript parent never
#     re-renders; the commit touches only the Files drawer.
#   - collapse → re-expand is ZERO new listing RPCs (cached dir state).
#   - drawer close + outer reopen restores the EXACT expansion
#     geometry (fresh listing is the contract; geometry lives in the view
#     store, and no `›`/`‹` collapse chrome remains) with the EXACT path-
#     correlated listing wire count: one file_explorer_list request for the
#     toggled path settled by one exact correlated success response.
#   - A/B session isolation: session B's drawer starts at the default
#     expansion; B's toggle never leaks into A and vice versa.
#   - SUBDIRECTORY TRANSCRIPT LINKS (FL6, after isolation; never the
#     1500-message parent):
#     FL6.1 control: a fresh Manage Session whose cwd is <workspace>/pkg
#     clicks nested/link-target.txt:1 and shows LINK-NESTED-EXACT.
#     FL6.2: session-a-seed cwd is the workspace root. The click reads the
#     shortened nested/link-target.txt first (that response fails), then
#     pkg/nested/link-target.txt (that response succeeds) and the content
#     shows LINK-NESTED-EXACT. A tree row is not required.
#     FL6.3 duplicate basename dup-name.txt issues no homonym read.
#     FL6.4 the absolute citation is a sibling of the workspace root
#     (<workspace>/../outside-sentinel/outside-only.txt). The click issues
#     no file_explorer_read and does not show LINK-OUTSIDE. A marker is
#     allowed.
#     FL6.5 the already-seeded workspace-root README.md (marker
#     FL-ROOT-README-EXACT on line 2) is cited as the BARE basename
#     `README.md:2` with NO longer path context: the render stamps the
#     exact basename (data-md-path, never data-md-resolved="none") and the
#     click opens it through session-a-seed's correlated session-stamped
#     file_explorer_read showing FL-ROOT-README-EXACT with no
#     file-unavailable error (no content error, no unavailable toast).
#   - JOURNEY SURFACES at both breakpoints: zero horizontal overflow (body, the .file-explorer shell, its header, #toasts) and zero visible error toasts.
#   - SCREENSHOTS: 1440x900 files-latency-open + -refresh-busy; 390x844 -mobile-open + -refresh-busy-mobile; plus -isolation after the A/B round-trip and -subdir-link after FL6.
#   - zero browser console/page/websocket errors across the whole lane.
#   - explicit exact assertion IDs (FL0.1-FL6.5, exact-once ledger) are
#     published in coverage-assertions.json inside the run directory
#     alongside the timings — never timings-only — and the coverage matrix
#     gates them.
#
# Browser driver: playwright via npm (ephemeral install) with a system
# Chrome/Chromium binary or playwright's bundled chromium. Missing node, a
# failed playwright install, or no usable Chromium FAILS the lane (exit 1,
# setup); assertion failures exit 2+.
#
# Usage: bash E2E.d/web/files_latency.sh [run|list]
set -euo pipefail
SCRIPT_DIR="$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
E2E_DIR="$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd -P)"
# shellcheck source=../lib/common.sh
. "$E2E_DIR/lib/common.sh"
# shellcheck source=lib/fixture.sh
. "$SCRIPT_DIR/lib/fixture.sh"

SCENARIO="web-files-latency"
TOKEN="web-files-latency-e2e-token-$$-$(date +%s)"
TOGGLE_DIR="mod7"
SECOND_DIR="mod3"
TOP_DIRS=10
SUBS_PER_DIR=5
FILES_PER_SUB=10   # 10 * 5 * 10 = 500 dirty files == the backend catalog cap
MESSAGES=1500

# ---------------------------------------------------------------------------
# Seed the fixture workspace: git repo, then 500 tracked-then-dirty files
# across a 10x5 deep tree so file_explorer_changes returns the full 500-entry
# capped catalog and the merged tree renders deep aggregate badges.
# ---------------------------------------------------------------------------
seed_workspace() {
    local ws="$1"
    git -C "$ws" init -q
    git -C "$ws" config user.email "web-files-latency@example.com"
    git -C "$ws" config user.name "Web Files Latency"
    git -C "$ws" config commit.gpgsign false
    printf 'files latency baseline\nFL-ROOT-README-EXACT\n' >"$ws/README.md"
    local d s f
    for d in $(seq 0 9); do
        for s in $(seq 0 $((SUBS_PER_DIR - 1))); do
            mkdir -p "$ws/mod$d/sub$s"
            for f in $(seq 0 $((FILES_PER_SUB - 1))); do
                printf 'export const mod%d_%d_%d = %d;\n' "$d" "$s" "$f" "$f" >"$ws/mod$d/sub$s/file-$f.ts"
            done
        done
    done
    git -C "$ws" add -- .
    git -C "$ws" commit -q -m baseline
    # Dirty: every leaf changes once -> exactly 500 changed files (deep paths).
    for d in $(seq 0 9); do
        for s in $(seq 0 $((SUBS_PER_DIR - 1))); do
            for f in $(seq 0 $((FILES_PER_SUB - 1))); do
                printf 'export const mod%d_sub%d_file%d = %d; // dirty\n' "$d" "$s" "$f" "$((f + 1))" >"$ws/mod$d/sub$s/file-$f.ts"
            done
        done
    done
}

# Subdirectory-cwd transcript-link fixture. The listener process cwd stays
# the repository root ($root/workspace). A later Manage Session -> New
# session selects $root/workspace/pkg, so file_explorer_read resolves
# against that session cwd. Unique names only — never a suffix of an
# existing latency tree path (modN/subN/file-N.ts).
seed_link_fixture() {
    local ws="$1"
    mkdir -p "$ws/pkg/nested" "$ws/pkg/a" "$ws/pkg/b" "$(dirname "$ws")/outside-sentinel"
    printf 'LINK-NESTED-EXACT\n' >"$ws/pkg/nested/link-target.txt"
    printf 'LINK-DUP-A\n' >"$ws/pkg/a/dup-name.txt"
    printf 'LINK-DUP-B\n' >"$ws/pkg/b/dup-name.txt"
    printf 'LINK-OUTSIDE\n' >"$(dirname "$ws")/outside-sentinel/outside-only.txt"
    git -C "$ws" add -- pkg
    git -C "$ws" commit -q -m 'transcript link fixture'
}

main() {
    case "${1:-run}" in
        list|--list|--dry-run)
            printf '%s\n' \
                'web-files-latency - Files drawer directory-toggle latency: real pointer click -> synchronous store write + file_explorer_list RPC send -> MutationObserver aria-expanded/child commit -> next rAF paint -> exact successful listing response; commit lands by the first animation frame and before the listing response; exactly one listing RPC on first expansion, zero on collapse/re-expand; 1500-message transcript parent render count unchanged during the toggle (React fiber-commit probe); drawer close/outer reopen restores exact expansion with no `›`/`‹` chrome; A/B session view isolation; observed durations reported to evidence, no invented pass threshold (PLAYWRIGHT-ONLY, hard-fail)'
            return 0
            ;;
        run|all) ;;
        *) fail "usage: $0 [run|list]" ;;
    esac

    require_rpi
    require_cmd node
    require_cmd git
    require_cmd python3

    local root evidence port url pw_status=0
    root="$(scenario_workspace "$SCENARIO")"
    evidence="$EVIDENCE_ROOT/$SCENARIO"
    mkdir -p "$root/workspace"
    printf '%s\n' "$TOKEN" >"$root/token"
    seed_workspace "$root/workspace"
    seed_link_fixture "$root/workspace"

    # Seed TWO native sessions (version-3 JSONL, same writer shape as the
    # projects lane): 1500 messages each. The listener explicitly resumes A;
    # B exists for A/B view isolation. Seeding writes the encoded session dirs
    # to $evidence/seed-dirs.json for the playwright env.
    python3 - "$root/home/.rpi/sessions" "$root/workspace" "$evidence" "$MESSAGES" <<'PY'
import datetime, json, pathlib, sys
sessions_root = pathlib.Path(sys.argv[1])
cwd = sys.argv[2]
evidence = pathlib.Path(sys.argv[3])
message_count = int(sys.argv[4])

def encoded(c: str) -> str:
    s = c
    if s.startswith('/') or s.startswith('\\'):
        s = s[1:]
    return '--' + s.replace('/', '-').replace('\\', '-').replace(':', '-') + '--'

def write_seed(sid: str, name: str, stamp: str) -> pathlib.Path:
    directory = sessions_root / encoded(cwd)
    directory.mkdir(parents=True, exist_ok=True)
    records = [
        {'type': 'session', 'version': 3, 'id': sid, 'timestamp': stamp, 'cwd': cwd},
        {'type': 'session_info', 'id': 'si-1', 'parentId': None, 'timestamp': stamp, 'name': name},
    ]
    parent = None
    for i in range(message_count):
        role = 'user' if i % 2 == 0 else 'assistant'
        text = f'{name} message {i:05d} — filler transcript line for the Files latency parent render probe.'
        message = {'role': role, 'content': [{'type': 'text', 'text': text}], 'timestamp': i + 1}
        if role == 'assistant':
            message.update({
                'api': 'openai-completions',
                'provider': 'user-steering',
                'model': 'mock',
                'stopReason': 'stop',
            })
        message_id = f'm{i}'
        records.append({
            'type': 'message', 'id': message_id, 'parentId': parent, 'timestamp': stamp,
            'message': message,
        })
        parent = message_id
    target = directory / f'{stamp[:10]}T000000-{sid}.jsonl'
    target.write_text(''.join(json.dumps(r, separators=(',', ':')) + '\n' for r in records), encoding='utf-8')
    return target

stamp = datetime.datetime.now(datetime.timezone.utc).isoformat().replace('+00:00', 'Z')
file_b = write_seed('session-b-seed', 'session-b-seed', stamp)
file_a = write_seed('session-a-seed', 'session-a-seed', stamp)
(evidence / 'seed-dirs.json').write_text(json.dumps({'cwd': cwd}, indent=2), encoding='utf-8')
PY

    port="$(MOCK_SCENARIO=sessions web_start_mock "$root" "$evidence")"
    WEB_SPAWN_RESUME=session-a-seed web_spawn_rpi "$root" "$evidence" "$port"
    url="$(web_wait_for_listener "$evidence")"
    log "$SCENARIO: listener at $url"

    web_run_playwright "$url" "$evidence" "$root/playwright" "$SCRIPT_DIR/files_latency_test.mjs" \
        "RPI_MESSAGES=$MESSAGES" \
        "RPI_TOGGLE_DIR=$TOGGLE_DIR" \
        "RPI_SECOND_DIR=$SECOND_DIR" \
        "RPI_LINK_CWD=$root/workspace/pkg" \
        "RPI_OUTSIDE_FILE=$(dirname "$root/workspace")/outside-sentinel/outside-only.txt" \
        || pw_status=$?
    if [ "$pw_status" -ne 0 ]; then
        fail "$SCENARIO: playwright lane failed (exit $pw_status) — files-latency lane is playwright-only"
    fi

    web_sanity_http "$url"
    web_finish_lane "$SCENARIO"
}

main "$@"