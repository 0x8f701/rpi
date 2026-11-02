#!/usr/bin/env bash
# Web code-review REPLY LIFECYCLE regression lane (playwright-only hard gate).
#
# Ticket: "Code Review reply terminal state clears activity" — a completed,
# failed, cancelled, or aborted review reply must clear its in-flight owner
# exactly once: no per-hunk streaming dot, no header active-replies label, no
# Abort action, an enabled composer, and a working NEXT comment.
#
# The lane runs the REAL `rpi --listen` binary on an ISOLATED random port with
# the loopback mock provider (steering scenario + MOCK_REVIEW_MARKDOWN), opens
# /web in a real browser, and drives the actual review surface. All waits are
# event-gated (waitForFunction with deadlines — ZERO fixed sleeps); every
# assertion hard-fails (any failure -> nonzero exit); no private
# address/session/token is carried (the URL comes from the fixture's random
# port; the session is the fixture's own).
#
# Asserted lifecycle contract:
#   A1  bootstrap authority: the active sidebar session id owns a get_state
#       response (fixture session is the authority)
#   A2  /code-review opens and settles with the controlled dirty fixture file
#       as a selectable row, and its hunk becomes selectable + composer ready
#   B1  normal slow completion: the first comment ("review markdown matrix",
#       odd request -> 3-chunk slow stream) shows the streaming Abort action,
#       then the SAME reply settles exactly once — Abort clears, per-hunk
#       streaming dot clears, header active-replies label clears, composer
#       re-enables, and exactly one assistant card holds the reply
#   B2  next prompt after completion: a second comment submits, completes,
#       and restores Comment/submit (composer enabled, no Abort)
#   B3  completion after a tool: "cr grep probe" drives ONE real grep tool
#       call and the echo reply commits; terminal state converges the same
#       way (no Abort, enabled composer)
#   B4  user abort: a slow stream is aborted via the Abort action; the abort
#       response settles terminal — Abort clears, composer restores, no
#       streaming dot, and the following comment still runs
#   B5  ordinary mid-stream lifecycle convergence: a later comment streams
#       again and the terminal projection converges the same way (Abort
#       clears, composer restores, panel stays open)
#   B6  zero browser console errors and zero page errors across the lane
#
# Browser driver: playwright via npm (ephemeral install) with a system
# Chrome/Chromium binary or playwright's bundled chromium. Missing node, a
# failed playwright install, or no usable Chromium FAILS the lane (exit 1,
# setup); assertion failures exit 2+.
#
# Usage: bash E2E.d/web/code_review_reply_lifecycle.sh [run|list]
set -euo pipefail
SCRIPT_DIR="$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
E2E_DIR="$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd -P)"
# shellcheck source=../lib/common.sh
. "$E2E_DIR/lib/common.sh"
# shellcheck source=lib/fixture.sh
. "$SCRIPT_DIR/lib/fixture.sh"

SCENARIO="web-code-review-reply-lifecycle"
TOKEN="web-code-review-reply-lifecycle-e2e-token-$$-$(date +%s)"
DIRTY_FILE="greet.txt"

# ---------------------------------------------------------------------------
# spawn `rpi --listen` with --approve (trusted project scope) on a kernel-
# assigned random port. Mirrors code_review_side_chat.sh's spawn_rpi.
# $1 root  $2 evidence. -> sets RPI_PID via register_pid.
# ---------------------------------------------------------------------------
spawn_rpi() {
    local root="$1" evidence="$2"
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
main() {
    case "${1:-run}" in
        list|--list|--dry-run)
            printf '%s\n' \
              'web-code-review-reply-lifecycle - reply terminal-state lifecycle: fixture-session get_state owner (A1), /code-review opens + dirty-file hunk composer (A2), slow reply streams with Abort then settles exactly once with Abort/dot/label cleared and composer restored (B1), next prompt runs (B2), completion after a real grep tool call converges (B3), user abort mid-stream settles terminal and restores submit (B4), a later mid-stream comment converges the same way (B5), zero browser console/page errors (B6); event-gated waits only, hard-fail assertions, isolated random-port listener (PLAYWRIGHT-ONLY, hard-fail)'
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

    # Seed a TEMP git repo (loopback fixture only) with a committed baseline
    # and a dirty tracked file so the code-review panel renders a REAL diff,
    # plus a "needle" line the B3 grep tool probe can genuinely match.
    git -C "$root/workspace" init -q
    git -C "$root/workspace" config user.email "web-e2e@example.com"
    git -C "$root/workspace" config user.name "Web E2E"
    git -C "$root/workspace" config commit.gpgsign false
    cat >"$root/workspace/$DIRTY_FILE" <<'EOF'
alpha
needle
beta
EOF
    printf 'fn main() {}\n// needle\n' >"$root/workspace/source.rs"
    git -C "$root/workspace" add -- .
    git -C "$root/workspace" -c commit.gpgsign=false commit -q -m baseline
    cat >"$root/workspace/$DIRTY_FILE" <<'EOF'
alpha
NEEDLE-CHANGED
beta
delta
EOF

    spawn_rpi "$root" "$evidence"
    url="$(web_wait_for_listener "$evidence")"
    log "$SCENARIO: listener at $url"

    local pw_status=0 pw_pid=0
    web_run_playwright "$url" "$evidence" "$root/playwright" \
        "$SCRIPT_DIR/code_review_reply_lifecycle_test.mjs" \
        RPI_DIRTY_FILE="$DIRTY_FILE" \
        >"$evidence/playwright.out" 2>&1 &
    pw_pid=$!

    wait "$pw_pid" || pw_status=$?
    if [ "$pw_status" -eq 0 ]; then
        printf 'playwright: code-review reply lifecycle (fixture-session owner; review opens + hunk composer; slow reply streams with Abort then settles exactly once with Abort/dot/label cleared and composer restored; next prompt runs; completion after real grep tool call converges; user abort settles terminal and restores submit; a later mid-stream comment converges the same way; zero browser errors): PASSED\n' \
            | tee "$evidence/playwright-summary.txt"
    elif [ "$pw_status" -eq 1 ]; then
        fail "$SCENARIO: playwright SETUP FAILED (node/chromium/npm unavailable)"
    else
        fail "$SCENARIO: playwright lane failed (exit $pw_status)"
    fi

    web_sanity_http "$url"
    web_finish_lane "$SCENARIO"
}

main "$@"