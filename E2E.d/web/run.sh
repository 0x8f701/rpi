#!/usr/bin/env bash
# Web client E2E suite runner (unified) — ONE command for the complete web
# regression suite (playwright-only hard gate).
#
# Runs every web lane in sequence, each with its own fixture (loopback mock
# provider + real `rpi --listen` binary + token file + real browser):
#
#   core      E2E.d/web/core.sh           load, subprotocol WS auth, prompt
#                                         round-trip, abort+recovery, todo
#                                         panel, rich content (table/task-
#                                         list/mermaid/KaTeX), workflow
#                                         create/cancel, settings, session,
#                                         subagents
#   goal      goal.sh                     Goal panel create/pin/pause/resume
#                                         + journal replay
#   xss       xss.sh                      hostile model output renders as
#                                         inert text + sk-* secret redacted
#   abort     abort.sh                    B1 early-abort preserves streamed
#                                         text; B2 neutral "run aborted" toast
#   reconnect reconnect.sh                server kill -> reconnecting pill ->
#                                         auto-reconnect + transcript survives
#   switch    switch.sh                   model + thinking-level switch
#                                         (set_model / set_thinking_level)
#   mobile    mobile.sh                   375x667 viewport shell contract
#   auth      auth.sh                     TOKENED listener: no-token silent
#                                         probe, wrong-token error toast,
#                                         good-token connect + round-trip
#   auth_tokenless auth_tokenless.sh      TOKENLESS listener: empty-token
#                                         boot auto-connect reaches connected
#                                         + prompt round-trip
#   sessions  sessions.sh                 multi-session (PLAYWRIGHT-ONLY):
#                                         concurrent streaming across
#                                         sessions, source-session event
#                                         routing, background unread +
#                                         clear-on-switch, authoritative
#                                         transcript restore, abort/toast
#                                         isolation, close busy refusal +
#                                         idle success, 8-session cap / no
#                                         eviction, Todo/Goal/Workflow
#                                         isolation, desktop rail
#                                         collapse/reopen, Android 390x844
#                                         drawer pick-close, header has no
#                                         feature buttons
#
# Every lane is a playwright-only hard gate: it requires node and a usable
# Chromium path (system Chrome/Chromium or playwright's bundled chromium) and
# FAILS — never skips — when node is missing, the ephemeral playwright install
# fails, or no browser is usable. Per-lane pass/fail + evidence paths are
# aggregated into $EVIDENCE_ROOT/web/REPORT.md; the runner exits non-zero when
# any lane failed.
#
# Usage: bash E2E.d/web/run.sh [run|list]
#   RPI_WEB_LANES   space-separated lane subset to run (default: all lanes)
set -euo pipefail
SCRIPT_DIR="$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
E2E_DIR="$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd -P)"
# shellcheck source=../lib/common.sh
. "$E2E_DIR/lib/common.sh"
# shellcheck source=lib/fixture.sh
. "$SCRIPT_DIR/lib/fixture.sh"

# Lanes are run as child processes; export the run identity so every lane
# writes evidence under the SAME root the report links to (otherwise each
# lane re-rolls E2E_RUN_ID and the REPORT.md paths would not resolve).
export E2E_RUN_ID EVIDENCE_ROOT WORK_ROOT RPI_BIN

# Every lane script in the suite (core.sh = the shared core lane).
LANES="core goal xss abort reconnect switch mobile auth auth_tokenless extras sessions"

list_lanes() {
    local lane
    for lane in $LANES; do
        bash "$SCRIPT_DIR/$lane.sh" list
    done
}

main() {
    case "${1:-run}" in
        list|--list|--dry-run)
            printf '%s\n' '[Web client suite (playwright-only hard gate)]'
            list_lanes
            return 0
            ;;
        run|all) ;;
        *) fail "usage: $0 [run|list]" ;;
    esac

    require_rpi
    # The web release lanes are playwright-only hard gates: node is required
    # up front (fail fast), and each lane verifies a usable Chromium path
    # after its ephemeral playwright install.
    web_require_browser

    local report="$EVIDENCE_ROOT/web/REPORT.md"
    mkdir -p "$EVIDENCE_ROOT/web"
    printf '%s\n' '# Web client E2E suite report' '' '| lane | result | evidence |' '|---|---|---|' >"$report"

    local lanes="${RPI_WEB_LANES:-$LANES}"
    local lane overall=0
    for lane in $lanes; do
        # Every listed lane must reach its playwright .mjs or fail: an
        # unknown lane name is a hard error, not a skip.
        [ -f "$SCRIPT_DIR/$lane.sh" ] || fail "web: unknown lane '$lane' (no $SCRIPT_DIR/$lane.sh)"
        log "web: lane '$lane' starting"
        local start=$SECONDS
        # The core lane lives in core.sh but keeps the evidence dir name "web".
        local lane_evidence="$EVIDENCE_ROOT/web-$lane"
        [ "$lane" = "core" ] && lane_evidence="$EVIDENCE_ROOT/web"
        if bash "$SCRIPT_DIR/$lane.sh" run; then
            web_lane_report "$report" "$lane" "PASS" "$lane_evidence"
            log "web: lane '$lane' PASSED ($((SECONDS - start))s)"
        else
            # Lanes cannot skip: any non-zero exit (setup failure or failed
            # assertions) fails the lane.
            local status=$?
            web_lane_report "$report" "$lane" "FAIL" "$lane_evidence"
            log "web: lane '$lane' FAILED (exit $status, $((SECONDS - start))s)"
            overall=1
        fi
    done

    web_summary "$report" || overall=1
    if [ "$overall" -eq 0 ]; then
        log "web: full suite PASSED (report: $report)"
    else
        log "web: full suite has FAILURES (report: $report)"
    fi
    exit "$overall"
}

main "$@"
