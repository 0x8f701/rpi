#!/usr/bin/env bash
# Web code-review tree/paging/comment-markdown regression lane
# (playwright-only hard gate).
#
# Spawns the real `rpi --listen` binary with a token file and the loopback
# mock provider (--scenario steering + MOCK_REVIEW_MARKDOWN so the assistant
# review reply carries the markdown matrix), opens `/web` in a real browser,
# and asserts the code-review regressions:
#   - the file list is a NESTED tree: directory rows
#     (li.code-review__tree-row[data-tree-kind="dir"] with
#     button.code-review__tree-dir + aria-expanded) wrap their files; clicking
#     a directory row collapses/expands its children (aria-expanded flips, the
#     child file rows disappear/reappear) — nested directories included
#   - the >4000-line fixture file renders its truncated banner + page status;
#     clicking .code-review__load-more grows the rendered line set past the
#     4000-line cap so a previously hidden line (changed-line-04001) appears;
#     .code-review__load-full reaches the full diff or the hard UI cap
#   - a zz-* oversize fixture pushes the combined patch past MAX_DIFF_BYTES so
#     the catalog emits EMPTY placeholders (hunks: [] + truncated): selecting
#     one auto-loads its diff without any Refresh/Load click, never claims
#     "No hunks in this file", and unknown-language bodies stay plain
#   - rust diff lines render hljs token spans with verbatim textContent
#     (line numbers/prefix/kind backgrounds unchanged) and a hostile
#     <script> diff line stays LITERAL text with no side effect
#   - a submitted comment carrying **bold**, a markdown list, a ```rust```
#     fence, and hostile <script>/<img onerror> HTML renders markdown
#     (strong / ul>li / pre>code.hljs.language-rust) while the hostile HTML
#     stays LITERAL text — no element is created, no script runs
#     (window.__crPwned stays undefined), no dialog opens — for BOTH the user
#     comment and the assistant review reply (routed by the mock marker)
#
# Browser driver: playwright via npm (ephemeral install) with a system
# Chrome/Chromium binary or playwright's bundled chromium. Missing node, a
# failed playwright install, or no usable Chromium FAILS the lane (exit 1,
# setup); assertion failures exit 2+.
#
# Usage: bash E2E.d/web/code_review_paging.sh [run|list]
set -euo pipefail
SCRIPT_DIR="$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
E2E_DIR="$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd -P)"
# shellcheck source=../lib/common.sh
. "$E2E_DIR/lib/common.sh"
# shellcheck source=lib/fixture.sh
. "$SCRIPT_DIR/lib/fixture.sh"

SCENARIO="web-code-review-paging"
TOKEN="web-code-review-paging-e2e-token-$$-$(date +%s)"
DIRTY_FILE="greet.txt"
BIG_FILE="big.txt"

# ---------------------------------------------------------------------------
# spawn `rpi --listen` with --approve (trusted project scope). Mirrors
# commands_review.sh's spawn_rpi.
# $1 root  $2 evidence  $3 mock port. -> sets RPI_PID via register_pid.
# ---------------------------------------------------------------------------
spawn_rpi() {
    local root="$1" evidence="$2" port="$3"
    local stdin_path="$root/stdin"
    rm -f "$stdin_path"
    mkfifo "$stdin_path"
    exec 9<>"$stdin_path"
    (
        cd "$root/workspace"
        exec env -i \
            HOME="$root/home" USERPROFILE="$root/home" \
            PATH="${PATH:-/usr/bin:/bin}" LANG="${LANG:-C.UTF-8}" LC_ALL="${LC_ALL:-C.UTF-8}" \
            PI_CODING_AGENT_DIR="$root/home/.pi/agent" \
            PI_OFFLINE=1 PI_SKIP_VERSION_CHECK=1 \
            RPI_WEB_DEV_DIR="${RPI_WEB_DEV_DIR:-}" \
            "$RPI_BIN" --offline --approve \
            --listen 127.0.0.1:0 \
            --listen-plaintext \
            --listen-token-file "$root/token" \
            --model user-steering/mock --api-key user-mock-key \
            <"$stdin_path" >"$evidence/rpi.stdout" 2>"$evidence/rpi.stderr"
    ) &
    register_pid $!
}

# ---------------------------------------------------------------------------
main() {
    case "${1:-run}" in
        list|--list|--dry-run)
            printf '%s\n' \
                'web-code-review-paging - nested file tree collapse/expand (dir rows + aria-expanded), >4000-line diff Load more grows past the cap (changed-line-04001 appears), Load full, empty globally-truncated placeholder auto-loads on selection without a click, hljs token spans on rust diff lines with verbatim textContent + literal hostile <script>, comment markdown bold/list/rust fence + hostile HTML literal with no side effect for user AND assistant comments (PLAYWRIGHT-ONLY, hard-fail)'
            return 0
            ;;
        run|all) ;;
        *) fail "usage: $0 [run|list]" ;;
    esac

    require_rpi
    web_require_browser
    require_cmd git

    local root evidence port url
    root="$(scenario_workspace "$SCENARIO")"
    evidence="$EVIDENCE_ROOT/$SCENARIO"
    printf '%s\n' "$TOKEN" >"$root/token"

    port="$(MOCK_REVIEW_MARKDOWN=1 web_start_mock "$root" "$evidence")"

    # Seed a TEMP git repo (loopback fixture only) with a committed baseline,
    # a dirty tracked file, and a NESTED directory structure so the review
    # panel renders a real tree (dirs "src" and "src/deep"), plus the
    # >4000-line oversize file the backend marks truncated.
    git -C "$root/workspace" init -q
    git -C "$root/workspace" config user.email "web-e2e@example.com"
    git -C "$root/workspace" config user.name "Web E2E"
    git -C "$root/workspace" config commit.gpgsign false
    cat >"$root/workspace/$DIRTY_FILE" <<'EOF'
alpha
beta
gamma
EOF
    mkdir -p "$root/workspace/src/deep"
    printf 'root helper\n' >"$root/workspace/lib.rs"
    printf 'other module\n' >"$root/workspace/src/other.rs"
    printf 'deep feature\n' >"$root/workspace/src/deep/feature.rs"
    printf 'later base\n' >"$root/workspace/zz-later.txt"
    printf 'zz-big2 base\n' >"$root/workspace/zz-big2.txt"
    # Oversize file: each changed line is ~196 bytes, so a 6000-line file's
    # diff exceeds MAX_DIFF_BYTES (2 MiB) on its own. Sorted last (zz-), it
    # forces the combined patch past the snapshot byte cap: every file AFTER
    # it in git's path-sorted output becomes an EMPTY placeholder (hunks: [],
    # truncated) that the frontend auto-loads on selection.
    ZBIG_LINES=6000
    awk -v n="$ZBIG_LINES" 'BEGIN{ pad="xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx"; for(i=1;i<=n;i++) printf "zz-big-line-%05d %s\n", i, pad }' >"$root/workspace/zz-big.txt"
    git -C "$root/workspace" add -- .
    git -C "$root/workspace" -c commit.gpgsign=false commit -q -m baseline
    # Dirty working tree: greet.txt changes + nested file changes.
    cat >"$root/workspace/$DIRTY_FILE" <<'EOF'
alpha
BETA
gamma
delta
EOF
    printf 'fn src_helper() -> u32 {\n    let cards = items.filter(|i| i.kind == "toolCard");\n    cards.count() as u32\n}\n<script>window.__crPwned=1</script>\n' >"$root/workspace/src/other.rs"
    printf 'deep feature changed\n' >"$root/workspace/src/deep/feature.rs"
    printf 'later changed\n' >"$root/workspace/zz-later.txt"
    printf 'zz-big2 changed\n' >"$root/workspace/zz-big2.txt"
    awk -v n="$ZBIG_LINES" 'BEGIN{ pad="yyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyy"; for(i=1;i<=n;i++) printf "zz-big-changed-%05d %s\n", i, pad }' >"$root/workspace/zz-big.txt"
    # Oversize file: > MAX_FILE_RENDER_LINES (4000) rendered diff lines so the
    # backend marks the FILE truncated (4100 changed lines => ~8200 diff lines).
    BIG_LINES=4100
    for i in $(seq 1 "$BIG_LINES"); do printf 'base-line-%05d\n' "$i"; done >"$root/workspace/$BIG_FILE"
    git -C "$root/workspace" add -- "$BIG_FILE"
    git -C "$root/workspace" -c commit.gpgsign=false commit -q -m "big baseline" -- "$BIG_FILE"
    for i in $(seq 1 "$BIG_LINES"); do printf 'changed-line-%05d\n' "$i"; done >"$root/workspace/$BIG_FILE"

    spawn_rpi "$root" "$evidence" "$port"
    url="$(web_wait_for_listener "$evidence")"
    log "$SCENARIO: listener at $url"

    local pw_status=0
    web_run_playwright "$url" "$evidence" "$root/playwright" \
        "$SCRIPT_DIR/code_review_paging_test.mjs" \
        RPI_DIRTY_FILE="$DIRTY_FILE" \
        RPI_BIG_FILE="$BIG_FILE" || pw_status=$?
    if [ "$pw_status" -eq 0 ]; then
        printf 'playwright: code-review tree + paging + comment markdown contract: PASSED\n' | tee "$evidence/playwright-summary.txt"
    elif [ "$pw_status" -eq 1 ]; then
        fail "$SCENARIO: playwright SETUP FAILED (node/chromium/npm unavailable)"
    else
        fail "$SCENARIO: playwright lane failed (exit $pw_status)"
    fi

    web_sanity_http "$url"
    web_finish_lane "$SCENARIO"
}

main "$@"
