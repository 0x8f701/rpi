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
#                                         subagents (running-detail modal)
#   goal      goal.sh                     Goal panel create/pin/pause/resume
#                                         + journal replay
#   xss       xss.sh                      hostile model output renders as
#                                         inert text + sk-* secret redacted
#   abort     abort.sh                    B1 early-abort preserves streamed
#                                         text; B2 neutral "run aborted" toast
#   scroll    scroll.sh                   streaming transcript bottom-pin
#                                         contract: pinned long stream,
#                                         viewport preserved on scroll-up,
#                                         re-pin on return, session switch
#                                         pins the activated transcript
#   reconnect reconnect.sh                server kill -> reconnecting pill ->
#                                         auto-reconnect + transcript survives
#   readygate readygate.sh                mount-before-WebSocket-OPEN gate:
#                                         delayed open + simulated reconnect,
#                                         sidebar/panel eventually load with
#                                         no `load failed: not connected`
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
#                                         isolation, 8-session cap / no
#                                         eviction, Todo/Goal/Workflow
#                                         isolation, desktop rail
#                                         collapse/reopen, Android 390x844
#                                         drawer pick-close, header has no
#                                         feature buttons
#   session_restore session_restore.sh       authoritative loaded/disk/restart
#                                         transcript restoration
#   projects  projects.sh                  all-project native session catalog
#                                         + cross-project New-session storage:
#                                         seeded A/B sessions listed as two
#                                         sidebar project subgroups under the
#                                         rpi provider, project-B switch
#                                         activates backend cwd B, New session
#                                         inherits B and records under B's
#                                         encoded default session dir (panel
#                                         path + on-disk file proof)
#   attachments attachments.sh            clipboard image paste (canceled for
#                                         files only, not for text), multi-
#                                         file picker (.rs+.ts), multi-file
#                                         drag/drop with drop-active highlight,
#                                         chip global intake order, PDF
#                                         rejection toast + no chip, outgoing
#                                         prompt WS frame: 1 image block +
#                                         code files in order + no PDF,
#                                         attachments clear after dispatch
#   commands_review commands_review.sh     composer command button left of
#                                         textarea, command picker lists
#                                         /compact /skill /code-review,
#                                         /code-review draft + no auto-submit,
#                                         Enter opens the real review panel
#                                         (HEAD→working tree + dirty file +
#                                         changed lines) + close, /skill
#                                         <fixture> visible summary, /compact
#                                         outgoing-WS dispatch
#   loop_goal loop_goal.sh               composer picker lists /loop + /goal
#                                         + /ps (get_commands authority),
#                                         draft-no-auto-submit, Enter
#                                         dispatches loop create/list/update/
#                                         delete/cancel + goal create/show/
#                                         pin/pins/pause/resume/complete/drop
#                                         + bare /ps (empty process list)
#                                         against the real listener (goal
#                                         create/resume activate work, TUI
#                                         parity), /loop create/update
#                                         streaming guard, local usage errors
#                                         with no RPC (/ps extra included),
#                                         no user bubbles, sessionId-stamped
#                                         frames
#   slowclient slowclient.sh              slow-client contract: delta-burst
#                                         calibration, heavy-transcript burst
#                                         amplification bound, controlled
#                                         main-thread stall vs sustained flood
#                                         (no false 1008 on transient stalls;
#                                         sustained no-read still surfaces the
#                                         real 1008 with the server's reason),
#                                         accurate close cause + no mislabeled
#                                         pending rejection, close toast never
#                                         hidden
#   external_sessions external_sessions.sh external session discovery/import:
#                                         Web default OMP/Codex/Grok discovery
#                                         + provider grouping (rpi/OMP/Codex/Grok), foreign click
#                                         activates an rpi native import copy,
#                                         foreign source bytes/mtime immutable,
#                                         lineage/native-copy reuse, no
#                                         duplicate imported/foreign rows,
#                                         sessionImportSources:[] native-only
#   presentation presentation.sh         presentation regressions:
#                                        Command card title "Command" +
#                                        two-line clamp + no raw args +
#                                        success-green/failure-red border;
#                                        process/write/read human summaries
#                                        with no raw JSON; process cards
#                                        equal width; process .op-missing
#                                        error; composer command/input/send
#                                        equal height (desktop) + buttons
#                                        equal height + textarea>=240
#                                        (mobile); thinking streaming-visible
#                                        then final-hidden; durable
#                                        bashExecution; session sidebar
#                                        provider grouping (rpi/Codex/Grok/
#                                        OMP only, no tmp/UUID) + search
#                                        filter/clear; inline image render
#                                        (naturalWidth>0); video controls +
#                                        preload=metadata + no autoplay;
#                                        hostile media rejected
#   realtime_webrtc realtime_webrtc.sh  REAL browser RTCPeerConnection
#                                        loopback (no FakePeerConnection):
#                                        ICE-gather-then-POST fix, oai-events
#                                        datachannel round-trip, remote audio
#                                        track, setupRealtimeCall end-to-end
#                                        + session.update over the channel
#   readygate_spawn readygate_spawn.sh   WS delayed-open/reconnect: real
#                                        server kill -> reconnecting with NO
#                                        "load failed: not connected" during
#                                        the outage (crossing the sidebar's
#                                        8s poll), same-port respawn ->
#                                        sidebar + session panel eventually
#                                        load without user action, sentinel
#                                        absent post-reconnect
#   code_review_paging code_review_paging.sh nested code-review file tree
#                                        collapse/expand (dir rows +
#                                        aria-expanded), >4000-line diff
#                                        Load more grows past the cap
#                                        (changed-line-04001 appears) + Load
#                                        full, comment markdown
#                                        bold/list/rust-fence + hostile HTML
#                                        literal for user AND assistant
#                                        comments
#   skill_completion skill_completion.sh /skill candidates come from the
#                                        REAL loaded disk catalog (TWO seeded
#                                        skills both render with frontmatter
#                                        descs), select greet -> /skill greet
#                                        no auto-submit -> frontmatter
#                                        summary bubble; no-skill workspace
#                                        shows "No skills loaded" + zero
#                                        candidates
#   realtime_rpc realtime_rpc.sh         user-visible realtime
#                                        start/error/stop against the REAL
#                                        Rust proxy: #mic-btn dispatches
#                                        realtime_create_call + mock records the
#                                        quicksilver JSON create-call,
#                                        live overlay ("realtime voice" +
#                                        conn-state bucket), mock-500 ->
#                                        "realtime call failed" toast +
#                                        overlay down, second click ->
#                                        realtime_stop + overlay gone
#   appborder appborder.sh               desktop dark+light .app-main
#                                        computed border (1px solid
#                                        --border-strong both edges,
#                                        header/footer edges, no rail seam,
#                                        stable transcript surface) + mobile
#                                        390x844 flush edges with zero
#                                        horizontal overflow
#   personas  personas.sh                persistent persona definitions:
#                                        list with memory/session counts,
#                                        view definition, select as
#                                        preferred, run (task_spawn with the
#                                        persona agent name), create
#                                        (catalog discoverable after the
#                                        config save), edit name-agreement
#                                        gate, remove-vs-purge confirmation
#                                        with on-disk containment, DOM
#                                        hygiene (no credentials, no
#                                        absolute paths)
#   stt_rpc stt_rpc.sh                  user-visible hold-to-talk STT
#                                        against the REAL Rust proxy:
#                                        synthetic-mic hold dispatches
#                                        stt_transcribe ({audioBase64,
#                                        mimeType} only — no URL/key), mock
#                                        /v1/audio/transcriptions records
#                                        server-held Bearer + multipart WAV +
#                                        model (metadata-only evidence),
#                                        transcript lands in composer, key
#                                        never in DOM/evidence; mock-500 ->
#                                        bounded transcription-failed toast
#                                        with no transcript
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
LANES="core goal xss abort scroll reconnect readygate switch mobile auth auth_tokenless extras sessions session_restore projects commands_review loop_goal attachments slowclient external_sessions presentation realtime_webrtc readygate_spawn code_review_paging skill_completion realtime_rpc appborder personas stt_rpc"

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
