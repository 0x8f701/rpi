#!/usr/bin/env bash
# Web code-review + side-chat interaction regression lane (playwright-only
# hard gate).
#
# PERMANENT repo-safe replacement for the deleted one-off probe
# (E2E.d/web/.omlo-code-review-probe.mjs): that probe hardcoded a private LAN
# endpoint, used fixed 35s sleeps, logged failures without failing, and
# always exited 0. This lane is the opposite — it runs the REAL `rpi
# --listen` binary on an ISOLATED random port with a token file and the
# loopback mock provider, opens `/web` in a real browser, waits ONLY on
# events (DOM/ws-frame waitForFunction with deadlines, zero fixed sleeps),
# hard-fails every assertion (any failure -> nonzero exit), and carries no
# private address/session/token (the URL comes from the fixture's random
# port; the session is the fixture's own).
#
# Asserted interaction contract:
#   A1  the fixture session is the authority: the active sidebar session id
#       is returned as the owner of a get_state response
#   A2  /code-review opens and settles with a REAL diff of the controlled
#       dirty fixture file (greet.txt)
#   A3  a post-code-review get_state round-trips on the SAME socket
#       generation (no transport churn from the review session)
#   A4  the Side Chat panel opens from #sidechat-toggle-btn, mounts
#       #side-chat-panel, and New creates an active tab with the composer ready
#   A5  a REAL listener kill produces exactly one close code===1006 and
#       exactly one transport toast; the lane respawns on the SAME port
#       (--resume the pre-outage primary) while the shim fails three product
#       reconnect attempts synchronously before any TCP socket opens (no
#       native ERR_CONNECTION_REFUSED is manufactured); the product recovers
#       on its own (generation advances, socket OPEN), the toast clears, and
#       the side-chat panel stays mounted with no inline/sidebar error
#   A6  ZERO browser console errors and ZERO page errors across the whole
#       lane (native ERR_CONNECTION_REFUSED handling is never loosened)
#
# The kill/respawn marker handshake mirrors reconnect.sh and
# code_review_paging.sh: the test writes kill-server.marker, this lane kills
# the listener, the test proves the 1006 + toast and writes
# outage-observed.marker, this lane respawns on the same port with --resume
# and touches server-up.marker.
#
# Self-test: INJECT_FAIL=1 flips an expected assertion value (the outage
# transport toast count) so the lane MUST fail — proof the assertions have
# teeth. Documented self-test flag; never used in the normal suite.
#
# Browser driver: playwright via npm (ephemeral install) with a system
# Chrome/Chromium binary or playwright's bundled chromium. Missing node, a
# failed playwright install, or no usable Chromium FAILS the lane (exit 1,
# setup); assertion failures exit 2+.
#
# Usage: bash E2E.d/web/code_review_side_chat.sh [run|list]
set -euo pipefail
SCRIPT_DIR="$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
E2E_DIR="$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd -P)"
# shellcheck source=../lib/common.sh
. "$E2E_DIR/lib/common.sh"
# shellcheck source=lib/fixture.sh
. "$SCRIPT_DIR/lib/fixture.sh"

SCENARIO="web-code-review-side-chat"
TOKEN="web-code-review-side-chat-e2e-token-$$-$(date +%s)"
DIRTY_FILE="greet.txt"

# ---------------------------------------------------------------------------
# spawn `rpi --listen` with --approve (trusted project scope) on a kernel-
# assigned random port. Mirrors commands_review.sh's spawn_rpi.
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
# respawn `rpi --listen` on a FIXED port after the A5 kill, resuming the
# pre-outage primary session (same-port recovery).
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
                'web-code-review-side-chat - permanent replacement for the deleted private probe: fixture-session get_state owner (A1), /code-review opens + real dirty-file diff (A2), post-CR get_state on the SAME generation (A3), side chat panel mounts + named chat created + active tab + composer ready (A4), real 1006 kill -> exactly one close/toast -> same-port --resume respawn -> recovery generation + panel stays mounted + toast clears (A5), zero browser console/page errors (A6); event-gated waits only, hard-fail assertions, isolated random-port listener (PLAYWRIGHT-ONLY, hard-fail)'
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
    # and a dirty tracked file so the code-review panel renders a REAL diff.
    git -C "$root/workspace" init -q
    git -C "$root/workspace" config user.email "web-e2e@example.com"
    git -C "$root/workspace" config user.name "Web E2E"
    git -C "$root/workspace" config commit.gpgsign false
    cat >"$root/workspace/$DIRTY_FILE" <<'EOF'
alpha
beta
gamma
EOF
    git -C "$root/workspace" add -- .
    git -C "$root/workspace" -c commit.gpgsign=false commit -q -m baseline
    cat >"$root/workspace/$DIRTY_FILE" <<'EOF'
alpha
BETA
gamma
delta
EOF

    spawn_rpi "$root" "$evidence" "$port"
    url="$(web_wait_for_listener "$evidence")"
    log "$SCENARIO: listener at $url"

    local pw_status=0 pw_pid=0
    web_run_playwright "$url" "$evidence" "$root/playwright" \
        "$SCRIPT_DIR/code_review_side_chat_test.mjs" \
        RPI_DIRTY_FILE="$DIRTY_FILE" \
        RPI_WORK="$root/playwright" \
        INJECT_FAIL="${INJECT_FAIL:-0}" \
        >"$evidence/playwright.out" 2>&1 &
    pw_pid=$!

    # A5: when the browser proves the real 1006 (it wrote kill-server.marker),
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
        local listen_port url2 outage_deadline pw_alive=1
        listen_port="$(printf '%s' "$url" | sed -n 's#http://[^:]*:\([0-9]*\)/web#\1#p')"
        log "$SCENARIO: test requested a real listener kill on port $listen_port"
        web_kill_rpi
        outage_deadline=$((SECONDS + 60))
        while [ ! -f "$root/playwright/outage-observed.marker" ] && [ "$SECONDS" -lt "$outage_deadline" ]; do
            if ! kill -0 "$pw_pid" 2>/dev/null; then pw_alive=0; break; fi
            sleep 0.1
        done
        if [ -f "$root/playwright/outage-observed.marker" ]; then
            local resume_sid
            resume_sid="$(cat "$root/playwright/listener-primary-session-id")"
            respawn_rpi "$root" "$evidence" "$port" "$listen_port" "$resume_sid"
            url2="$(web_wait_for_listener "$evidence" "2")"
            log "$SCENARIO: listener recovered at $url2"
            touch "$root/playwright/server-up.marker"
        elif [ "$pw_alive" -eq 0 ]; then
            # The playwright test died before proving the close (e.g. an
            # assertion failed while the outage was in flight): its own exit
            # status and stdout carry the truthful failure — do not mask it
            # with the marker message below.
            :
        else
            fail "$SCENARIO: browser did not prove the real listener close code 1006"
        fi
    fi

    wait "$pw_pid" || pw_status=$?
    if [ "$pw_status" -eq 0 ]; then
        printf 'playwright: code-review + side-chat interaction (fixture-session get_state owner; real dirty-file diff; same-generation post-CR get_state; side chat panel + named tab + composer; real 1006 kill + same-port respawn recovery with panel mounted and toast cleared; zero browser errors): PASSED\n' | tee "$evidence/playwright-summary.txt"
    elif [ "$pw_status" -eq 1 ]; then
        fail "$SCENARIO: playwright SETUP FAILED (node/chromium/npm unavailable)"
    else
        fail "$SCENARIO: playwright lane failed (exit $pw_status)"
    fi

    web_sanity_http "$url"
    web_finish_lane "$SCENARIO"
}

main "$@"
