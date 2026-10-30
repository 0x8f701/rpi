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
#   - the >4000-line fixture pages WITHOUT any manual affordance: there is
#     no .code-review__load-more/.code-review__load-full button, no
#     truncated banner, and no bounded page status; the selection
#     eager-fetches the backend pages (code_review_file_diff cursors
#     0→1000→…→8000, terminal hasMore=false) and scrolling the real
#     .code-review__diff viewport near its bottom grows the visible DOM
#     window 4000→8000→8200 until the deepest fixture line
#     (changed-line-04100) renders
#   - a >20000-line diff fixture (complete body, well under every backend
#     cap) pins at the UI display cap after scrolling and renders the
#     precise status "Loaded 20,000 of 44,000 lines — the UI display limit
#     is 20,000 lines."; the header shows the fixture's authoritative
#     +N/-M totals and never a `· truncated` chip from the global preview
#     cap; the globally >2 MiB many-file fixture's empty placeholder
#     auto-loads via exactly one terminal backend page and renders no
#     truncated marker
#   - a zz-* oversize fixture pushes the combined patch past MAX_DIFF_BYTES so
#     the catalog emits EMPTY placeholders (hunks: [] + truncated): selecting
#     one auto-loads its diff without any Refresh/Load click, never claims
#     "No hunks in this file", and unknown-language bodies stay plain
#   - a per-file byte-cap fixture (its own diff exceeds the backend
#     MAX_FILE_DIFF_BYTES, 8 MiB) loads as an empty on-demand placeholder and
#     its terminal backend page reports byteCapped=true over the parsed line
#     count; the pane then renders the precise byte-cap status — distinct
#     from the UI display cap status asserted above
#   - rust diff lines render hljs token spans with verbatim textContent
#     (line numbers/prefix/kind backgrounds unchanged) and a hostile
#     <script> diff line stays LITERAL text with no side effect
#   - a submitted comment carrying **bold**, a markdown list, a ```rust```
#     fence, and hostile <script>/<img onerror> HTML renders markdown
#     (strong / ul>li / pre>code.hljs.language-rust) while the hostile HTML
#     stays LITERAL text — no element is created, no script runs
#     (window.__crPwned stays undefined), no dialog opens — for BOTH the user
#     comment and the assistant review reply (routed by the mock marker)
#   - /code-review HEAD (single base) opens as HEAD→working tree with
#     code_review_open from=HEAD includeWorkingTree=true (no `to` key);
#     a `cr grep probe` comment drives the mock tool path: live
#     streaming-dot + Abort, then terminal idle with cr-grep-echo plus
#     the RAW grep needle + matched path (stub/fail fails); a genuine
#     live snapshot poll is held unchanged (request id + send/capture
#     times tracked) and delivered only after terminal — delivery fails
#     if the 30s pending timeout could expire it, and the App's pending
#     settlement proves the response was consumed, never a shim-only
#     delivery; overlay stays idle; reconnect stays idle
#   - a >30s delayed code_review_open (RPI_OPEN_DELAY_MS, default 35s —
#     past the old 30s fast-ack horizon) never tears down the transport:
#     the socket generation stays constant across the whole delay with zero
#     closes, an unrelated get_state succeeds mid-delay and again past 30s,
#     the heartbeat's JSON pong proves liveness during the delay, no toast
#     or inline transport error appears, and the panel settles with the
#     HEAD→working tree snapshot once the response lands; a REAL abnormal
#     close — the lane KILLS the rpi listener so the transport is severed at
#     the socket level (no synthetic close(code), so the close handshake can
#     never complete and Chromium reports the genuine abnormal-closure code
#     1006) — then produces exactly one transport toast and exactly one close
#     with code === 1006 on the killed generation, the lane respawns the
#     listener on the SAME port (--resume the pre-outage primary) while the
#     shim fails three product reconnect attempts synchronously (no native
#     ERR_CONNECTION_REFUSED is manufactured), and the panel reconciles and
#     stays mounted with no inline/sidebar error — with ZERO browser
#     console/page errors across the segment (native ERR_CONNECTION_REFUSED
#     handling is never loosened; the drop is a real 1006, not a refused
#     connect)
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
UICAP_FILE="uicap.txt"
BYTECAP_FILE="zz-bytecap.txt"
NAV_FILE="nav.txt"

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
            RPI_HOME="$root/home/.rpi" \
            RPI_OFFLINE=1 RPI_SKIP_VERSION_CHECK=1 \
            RPI_WEB_DEV_DIR="${RPI_WEB_DEV_DIR:-}" \
            "$RPI_BIN" --offline --approve \
            --listen 127.0.0.1:0 \
            --listen-plaintext \
            --listen-token-file "$root/token" \
            --model user-steering/mock --api-key user-mock-key \
            <"$stdin_path" >"$evidence/rpi.stdout" 2>"$evidence/rpi.stderr"
    ) &
    RPI_PID=$!
    register_pid $!
}

# ---------------------------------------------------------------------------
# respawn `rpi --listen` on a FIXED port after the D6 kill, resuming the
# pre-outage primary session (the delayed-open lane's same-port recovery).
# $1 root  $2 evidence  $3 mock port  $4 fixed listen port  $5 primary sid.
# -> sets RPI_PID. stderr lands in rpi2.stderr for web_wait_for_listener 2.
# ---------------------------------------------------------------------------
respawn_rpi() {
    local root="$1" evidence="$2" port="$3" listen_port="$4" sid="$5"
    local stdin_path="$root/stdin2"
    rm -f "$stdin_path"
    mkfifo "$stdin_path"
    exec 10<>"$stdin_path"
    (
        cd "$root/workspace"
        exec env -i \
            HOME="$root/home" USERPROFILE="$root/home" \
            PATH="${PATH:-/usr/bin:/bin}" LANG="${LANG:-C.UTF-8}" LC_ALL="${LC_ALL:-C.UTF-8}" \
            RPI_HOME="$root/home/.rpi" \
            RPI_OFFLINE=1 RPI_SKIP_VERSION_CHECK=1 \
            RPI_WEB_DEV_DIR="${RPI_WEB_DEV_DIR:-}" \
            "$RPI_BIN" --offline --approve \
            --listen "127.0.0.1:$listen_port" \
            --listen-plaintext \
            --listen-token-file "$root/token" \
            --resume "$sid" \
            --model user-steering/mock --api-key user-mock-key \
            <"$stdin_path" >"$evidence/rpi2.stdout" 2>"$evidence/rpi2.stderr"
    ) &
    RPI_PID=$!
    register_pid $!
}

# ---------------------------------------------------------------------------
main() {
    case "${1:-run}" in
        list|--list|--dry-run)
            printf '%s\n' \
                'web-code-review-paging - nested file tree collapse/expand (dir rows + aria-expanded), authoritative header +N/-M totals with no `· truncated` chip, >4000-line diff pages with NO Load buttons (eager backend pages with exact cursor progression 0→…→8000, scroll-grown DOM window to the terminal line), >20000-line UI-cap fixture pins at the cap with the precise Loaded N of M status, per-file byte-cap fixture (its own diff exceeds the backend 8 MiB cap) loads on selection and its terminal page reports byteCapped=true with the distinct byte-cap status, empty globally-truncated placeholder auto-loads on selection without a click, hljs token spans on rust diff lines with verbatim textContent + literal hostile <script>, comment markdown bold/list/rust fence + hostile HTML literal with no side effect for user AND assistant comments, /code-review HEAD single-base open + tool-path live Abort/dot then terminal idle + late rewritten snapshot stays idle + reconnect idle, >30s delayed code_review_open keeps the SAME socket generation alive (zero closes, mid+late get_state round-trips, JSON pong, no toast/inline error, panel settles) then a real 1006 drop + reconnect restores the panel with exactly one close/toast and no browser errors (PLAYWRIGHT-ONLY, hard-fail)'
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
    # >4000-line and >20000-line paging fixtures.
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
    for i in $(seq 1 120); do printf 'nav-base-%03d\n' "$i"; done >"$root/workspace/$NAV_FILE"
    # Oversize file: each changed line is ~196 bytes, so a 6000-line file's
    # diff exceeds MAX_DIFF_BYTES (2 MiB) on its own. Sorted last (zz-), it
    # forces the combined patch past the snapshot byte cap: every file AFTER
    # it in git's path-sorted output becomes an EMPTY placeholder (hunks: [],
    # truncated) that the frontend auto-loads on selection.
    ZBIG_LINES=6000
    awk -v n="$ZBIG_LINES" 'BEGIN{ pad="xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx"; for(i=1;i<=n;i++) printf "zz-big-line-%05d %s\n", i, pad }' >"$root/workspace/zz-big.txt"
    # Backend per-file byte-cap fixture: 8000 lines with ~600-char pads.
    # Committed at baseline and then rewritten, so the file's tracked
    # modification diff carries BOTH sides (~10 MiB) and exceeds
    # MAX_FILE_DIFF_BYTES (8 MiB) — the bounded per-file git run truncates
    # and every page reports byteCapped=true. Path-sorted after
    # zz-big2.txt, the combined 2 MiB snapshot patch carries none of it (an
    # empty on-demand placeholder like zz-later.txt): selecting it loads the
    # per-file pages and the pane renders the precise byte-cap status —
    # distinct from the UI display cap.
    BYTECAP_LINES=8000
    awk -v n="$BYTECAP_LINES" 'BEGIN{ pad=""; for(j=0;j<60;j++) pad = pad "xxxxxxxxxx"; for(i=1;i<=n;i++) printf "zz-bytecap-base-%05d %s\n", i, pad }' >"$root/workspace/$BYTECAP_FILE"
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
    printf 'deep feature changed needle\n' >"$root/workspace/src/deep/feature.rs"
    printf 'later changed\n' >"$root/workspace/zz-later.txt"
    printf 'zz-big2 changed\n' >"$root/workspace/zz-big2.txt"
    awk -v n="$ZBIG_LINES" 'BEGIN{ pad="yyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyy"; for(i=1;i<=n;i++) printf "zz-big-changed-%05d %s\n", i, pad }' >"$root/workspace/zz-big.txt"
    for i in $(seq 1 120); do
        if [ "$i" -ge 2 ] && [ "$i" -le 62 ]; then
            printf 'nav-changed-%03d\n' "$i"
        elif [ "$i" -eq 110 ]; then
            printf 'nav-second-hunk\n'
        else
            printf 'nav-base-%03d\n' "$i"
        fi
    done >"$root/workspace/$NAV_FILE"
    # Paging fixture: > MAX_FILE_RENDER_LINES (4000) rendered diff lines
    # (4100 changed lines => ~8200 diff lines) with a COMPLETE snapshot
    # body — ordinary bounded paging, no truncation banner, scroll-driven
    # DOM window growth.
    BIG_LINES=4100
    for i in $(seq 1 "$BIG_LINES"); do printf 'base-line-%05d\n' "$i"; done >"$root/workspace/$BIG_FILE"
    git -C "$root/workspace" add -- "$BIG_FILE"
    git -C "$root/workspace" -c commit.gpgsign=false commit -q -m "big baseline" -- "$BIG_FILE"
    for i in $(seq 1 "$BIG_LINES"); do printf 'changed-line-%05d\n' "$i"; done >"$root/workspace/$BIG_FILE"
    # UI display-cap fixture: 22000 changed lines => 44000 diff lines — a
    # COMPLETE body (no backend cap anywhere near it) that still exceeds the
    # frontend's FILE_DIFF_MAX_UI_LINES (20000): scrolling grows the window
    # up to the cap, then the pane reports the precise bounded status.
    # Short lines keep the diff far below the per-file byte cap and inside
    # the combined-patch budget before zz-big.txt forces the global preview
    # cut (zz-big2/zz-later stay empty on-demand placeholders).
    UICAP_LINES=22000
    for i in $(seq 1 "$UICAP_LINES"); do printf 'uicap-base-%05d\n' "$i"; done >"$root/workspace/$UICAP_FILE"
    git -C "$root/workspace" add -- "$UICAP_FILE"
    git -C "$root/workspace" -c commit.gpgsign=false commit -q -m "uicap baseline" -- "$UICAP_FILE"
    for i in $(seq 1 "$UICAP_LINES"); do printf 'uicap-changed-%05d\n' "$i"; done >"$root/workspace/$UICAP_FILE"
    awk -v n="$BYTECAP_LINES" 'BEGIN{ pad=""; for(j=0;j<60;j++) pad = pad "yyyyyyyyyy"; for(i=1;i<=n;i++) printf "zz-bytecap-changed-%05d %s\n", i, pad }' >"$root/workspace/$BYTECAP_FILE"
    # Authoritative header totals, computed from the same source the backend
    # uses (git diff --numstat over HEAD → working tree): the lane asserts
    # the header shows exactly these totals while the combined patch is
    # globally truncated (totalsComplete provenance, no `· truncated` chip).
    expected_totals="$(git -C "$root/workspace" diff --numstat HEAD \
        | awk '{ins += $1; del += $2} END {printf "+%d / -%d", ins, del}')"

    spawn_rpi "$root" "$evidence" "$port"
    url="$(web_wait_for_listener "$evidence")"
    log "$SCENARIO: listener at $url"

    local pw_status=0 pw_pid=0
    web_run_playwright "$url" "$evidence" "$root/playwright" \
        "$SCRIPT_DIR/code_review_paging_test.mjs" \
        RPI_DIRTY_FILE="$DIRTY_FILE" \
        RPI_BIG_FILE="$BIG_FILE" \
        RPI_UICAP_FILE="$UICAP_FILE" \
        RPI_BYTECAP_FILE="$BYTECAP_FILE" \
        RPI_NAV_FILE="$NAV_FILE" \
        RPI_EXPECTED_TOTALS="$expected_totals" \
        RPI_OPEN_DELAY_MS="${RPI_OPEN_DELAY_MS:-35000}" \
        RPI_WORK="$root/playwright" \
        >"$evidence/playwright.out" 2>&1 &
    pw_pid=$!

    # D6/D7: when the browser proves the real 1006 (it wrote kill-server.marker),
    # kill the listener so the transport is severed at the socket level, then
    # respawn on the SAME port with --resume of the pre-outage primary session.
    # The test drives three shimmed failed reconnect generations while this
    # shell restores the port, keeping Chromium's console clean without
    # loosening native ERR_CONNECTION_REFUSED handling.
    local kill_deadline=$((SECONDS + 240))
    while [ ! -f "$root/playwright/kill-server.marker" ] && [ "$SECONDS" -lt "$kill_deadline" ]; do
        kill -0 "$pw_pid" 2>/dev/null || break
        sleep 0.2
    done
    if [ -f "$root/playwright/kill-server.marker" ]; then
        local listen_port url2 outage_deadline
        listen_port="$(printf '%s' "$url" | sed -n 's#http://[^:]*:\([0-9]*\)/web#\1#p')"
        log "$SCENARIO: test requested a real listener kill on port $listen_port"
        web_kill_rpi
        outage_deadline=$((SECONDS + 60))
        while [ ! -f "$root/playwright/outage-observed.marker" ] && [ "$SECONDS" -lt "$outage_deadline" ]; do
            kill -0 "$pw_pid" 2>/dev/null || break
            sleep 0.1
        done
        [ -f "$root/playwright/outage-observed.marker" ] \
            || fail "$SCENARIO: browser did not prove the real listener close code 1006"
        local resume_sid
        resume_sid="$(cat "$root/playwright/listener-primary-session-id")"
        respawn_rpi "$root" "$evidence" "$port" "$listen_port" "$resume_sid"
        url2="$(web_wait_for_listener "$evidence" "2")"
        log "$SCENARIO: listener recovered at $url2"
        touch "$root/playwright/server-up.marker"
    fi

    wait "$pw_pid" || pw_status=$?
    if [ "$pw_status" -eq 0 ]; then
        printf 'playwright: code-review tree + scroll-driven paging (no Load buttons, eager backend pages with exact cursor progression, scroll-grown windows to the terminal line, >20000-line UI-cap Loaded-N-of-M status, per-file byte-cap byteCapped=true terminal page with the distinct byte-cap status, authoritative header totals, no `· truncated` chip) + comment markdown + /code-review HEAD + tool-path terminal lifecycle + late idle snapshot + reconnect idle + >30s delayed-open (same generation, zero closes, mid+late get_state, JSON pong) + real-1006 kill + same-port respawn recovery (panel reconciled, exactly one toast, zero browser errors): PASSED\n' | tee "$evidence/playwright-summary.txt"
    elif [ "$pw_status" -eq 1 ]; then
        fail "$SCENARIO: playwright SETUP FAILED (node/chromium/npm unavailable)"
    else
        fail "$SCENARIO: playwright lane failed (exit $pw_status)"
    fi

    web_sanity_http "$url"
    web_finish_lane "$SCENARIO"
}

main "$@"
