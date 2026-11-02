#!/usr/bin/env bash
# Web client E2E suite runner (unified) — ONE command for the complete web
# regression suite (playwright-only hard gate).
#
# Prepares the current-tree web bundle and playwright+Chromium once, then
# runs independent ordinary lanes with bounded concurrency. Coverage and
# pressure/stress/resource-sensitive lanes stay serial. Each lane still
# owns its own fixture (loopback mock provider + real `rpi --listen`
# binary + token file + real browser):
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
#   reconnect reconnect.sh                server kill -> transport outage
#                                         toast -> auto-reconnect + transcript
#                                         survives
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
#                                         isolation, 16-runtime cap + idle
#                                         LRU eviction, Todo/Goal/Workflow
#                                         isolation, desktop rail
#                                         collapse/reopen, Android 390x844
#                                         drawer pick-close, header has no
#                                         feature buttons
#   sessions_workflow sessions_workflow.sh    workflow DAG acceptance
#                                         (PLAYWRIGHT-ONLY, RPC-only, no
#                                         composer prompts/streaming races):
#                                         fail-closed planning probe lands
#                                         TERMINAL failed with a retained
#                                         non-empty reason, healthy
#                                         compile->ship DAG completes with
#                                         an applied integration, held
#                                         waves show live progress (never
#                                         completed while a task is open),
#                                         exact-once counts + task
#                                         ownership, workflow_list session
#                                         isolation, completed terminal
#                                         across re-polls
#   session_restore session_restore.sh       authoritative loaded/disk/restart
#                                         transcript restoration
#   session_ux session_ux.sh                 session UX matrix (PLAYWRIGHT-
#                                         ONLY, isolated fixture): seeded
#                                         native rows + one foreign Pi row,
#                                         keyboard rename accumulates per
#                                         character and sends exactly one
#                                         set_session_name frame (no
#                                         sessionPath), right-click Copy
#                                         context emits one
#                                         session_copy_context frame with
#                                         the seeded sanitized
#                                         conversation/goal/todo under the
#                                         64KiB bound + "Copied context"
#                                         toast without switching sessions,
#                                         oversized conversation truncates,
#                                         foreign rows expose Export HTML
#                                         only, zero page/console errors
#   sidebar_menu_rename sidebar_menu_rename.sh
#                                         sidebar PORTAL rename regression
#                                         (PLAYWRIGHT-ONLY): right-click a
#                                         SessionSidebar row -> Rename, then
#                                         natural keyboard typing
#                                         accumulates per character with
#                                         zero per-keystroke select/focus
#                                         calls and Enter commits the exact
#                                         typed name (portal
#                                         .session-sidebar__menu-rename
#                                         input the Session-panel input
#                                         cannot cover)
#   sidebar_rail sidebar_rail.sh           desktop collapsed-rail reopen hit
#                                         target (PLAYWRIGHT-ONLY): real
#                                         click on #rail-reopen-btn restores
#                                         the drawer, Enter/Space on the
#                                         focused button restore it, the
#                                         collapse/reopen cycle repeats, and
#                                         narrow mobile (<720px) drawer
#                                         behavior is unaffected
#   projects  projects.sh                  all-project native session catalog
#                                         + cross-project New-session storage:
#                                         seeded A/B sessions listed as two
#                                         sidebar project subgroups under the
#                                         rpi provider, project-B switch
#                                         activates backend cwd B, explicit-
#                                         directory New session records under
#                                         the selected project's encoded
#                                         default session dir (panel path +
#                                         on-disk file proof)
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
#                                         outgoing-WS dispatch, File Explorer
#                                         1360/900/390 Git M/A/D/R tree +
#                                         infinite scroll + hide reclaim
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
#                                         Web default discovery lists ALL
#                                         provider groups (rpi/Pi/OMP/Codex/
#                                         Grok) plus the default-collapsed
#                                         Agent sessions subgroup (bootstrap-
#                                         only rows hidden, search-revealed,
#                                         active/loaded force visible); a
#                                         foreign click activates an rpi
#                                         native import copy, foreign source
#                                         bytes/mtime immutable, lineage/
#                                         native-copy reuse, no duplicate
#                                         imported/foreign rows,
#                                         sessionImportSources:[] native-only;
#                                         ONE lane-set commit per run: both
#                                         phases stage into one shared
#                                         nonce-derived staging set, then the
#                                         shell writes merged coverage inside
#                                         the shared stage and commits ONE
#                                         stage-dir->run-dir rename + ONE
#                                         current.json pointer (no phase
#                                         pointers) with exact per-phase
#                                         counts + exact full union
#   files_latency files_latency.sh         Files-drawer directory-toggle
#                                         latency (PLAYWRIGHT-ONLY, SERIAL,
#                                         no invented ms budget): real
#                                         pointer click -> store write + one
#                                         file_explorer_list RPC -> DOM
#                                         commit by the first animation
#                                         frame -> exact correlated success
#                                         response AFTER the commit (all
#                                         durations from responseFrame.t
#                                         arithmetic, finite/nonnegative
#                                         gated); zero listing RPCs on
#                                         collapse/re-expand; 1500-message
#                                         transcript parent never
#                                         re-renders during the toggle
#                                         (fiber-commit probe); close/reopen
#                                         restores exact expansion with the
#                                         EXACT path-correlated listing
#                                         request/response count (1 request
#                                         + 1 success response); A/B
#                                         session view isolation; zero
#                                         console/page/websocket errors;
#                                         evidence published set-atomically
#                                         as one immutable run dir +
#                                         current.json manifest/pointer
#                                         after all gates, carrying explicit
#                                         FL assertion IDs + exact-once
#                                         counts (coverage-matrix gated)
#   switch_catalog_identity switch_catalog_identity.sh
#                                         negotiated catalog-identity switch
#                                         (PLAYWRIGHT-ONLY, SERIAL): seeded
#                                         1500-message A/B fixtures; COLD
#                                         switches (bootstrap restore AND
#                                         first user B->A click) dispatch
#                                         exactly ONE correlated
#                                         switch_session + one subscribe +
#                                         one authoritative cold-full cut,
#                                         while repeated exact-ready A-B-A
#                                         switches are ZERO-wire one-
#                                         animation-frame cutover with
#                                         composer/preference ownership; no
#                                         header session-name element
#                                         (desktop + mobile)
#   session_move_delete session_move_delete.sh
#                                         SessionSidebar Move/Delete
#                                         eligibility (PLAYWRIGHT-ONLY,
#                                         SERIAL): active row disables
#                                         Delete/Move, non-active
#                                         loaded/unloaded eligible, busy
#                                         refusal = one toast + zero catalog
#                                         mutation, idle delete + 5s Undo
#                                         restore, move relocates a saved
#                                         session under another project with
#                                         one session_move + catalog refresh
#                                         + active retention, foreign rows
#                                         export-only, desktop + mobile
#                                         (drawer + touch long-press)
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
#                                        scroll-driven paging with NO Load
#                                        buttons (exact cursor chain
#                                        0→…→8000), >20000-line UI-cap
#                                        Loaded-N-of-M status, per-file
#                                        byte-cap byteCapped=true status,
#                                        comment markdown
#                                        bold/list/rust-fence + hostile HTML
#                                        literal for user AND assistant
#                                        comments, /code-review HEAD
#                                        single-base open + tool-path live
#                                        Abort/dot then terminal idle + late
#                                        rewritten snapshot stays idle +
#                                        reconnect idle
#   code_review_side_chat code_review_side_chat.sh
#                                         code-review + side-chat interaction
#                                         regression (PLAYWRIGHT-ONLY,
#                                         SERIAL): fixture-session get_state
#                                         owner, /code-review opens with the
#                                         real dirty-file diff, post-review
#                                         get_state on the SAME socket
#                                         generation, side-chat panel mounts
#                                         + named chat created + active tab
#                                         + composer ready, a REAL listener
#                                         kill yields exactly one close 1006
#                                         + one transport toast and the
#                                         same-port --resume respawn recovers
#                                         on its own with the panel still
#                                         mounted, zero browser console/page
#                                         errors
#   chat_mermaid chat_mermaid.sh           /chat SessionView mermaid
#                                         (playwright-only hard gate): async
#                                         mermaid hydration stays bottom-
#                                         pinned, narrow diagrams center
#                                         without wide classification, user
#                                         upscroll is never stolen while new
#                                         replies stream, wide diagrams get
#                                         data-mermaid-wide + horizontal
#                                         scroll and re-pin, mobile 390x844
#                                         geometry, zero console/page errors
#   skill_completion skill_completion.sh /skill candidates come from the
#                                        REAL loaded disk catalog (TWO seeded
#                                        skills both render with frontmatter
#                                        descs), select greet -> /skill greet
#                                        no auto-submit -> frontmatter
#                                        summary bubble; no-skill workspace
#                                        shows "No skills loaded" + zero
#                                        candidates
#   plugin_command plugin_command.sh     trusted marketplace plugin command
#                                        in the Web picker + composer submit
#                                        dispatches RPC run and paints pong
#   processes processes.sh                Processes panel: sidebar toggle
#                                        opens #processes-panel, Run
#                                        dispatches process_spawn, Stop
#                                        dispatches process_stop, desktop
#                                        + mobile 390x844
#   code_review_reply_lifecycle code_review_reply_lifecycle.sh reply
#                                        terminal-state lifecycle: slow reply
#                                        streams with Abort then settles
#                                        exactly once (Abort/dot/label
#                                        cleared, composer restored), next
#                                        prompt, completion after a real
#                                        grep tool call, user abort settles
#                                        terminal + submit restored, later
#                                        mid-stream comment converges the
#                                        same way, zero browser errors
#   realtime_rpc realtime_rpc.sh         user-visible realtime
#                                        start/error/stop against the REAL
#                                        Rust proxy: #mic-btn dispatches
#                                        realtime_create_call + mock validates
#                                        the JSON sdp and complete session,
#                                        live overlay ("realtime voice" +
#                                        conn-state bucket), mock-500 ->
#                                        "realtime call failed" toast + overlay
#                                        down, second click -> realtime_stop +
#                                        overlay gone
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
#   chat      chat.sh                    durable /chat runner registry against
#                                        two real isolated listeners: scoped
#                                        auth, probe, reload, independent
#                                        reconnect, and deletion cleanup
#   todo_child_session todo_child_session.sh
#                                        real Todo-child Web session
#                                        (PLAYWRIGHT-ONLY hard gate):
#                                        token-authenticated rpi --listen
#                                        + sessions mock + real browser;
#                                        typed todo_child_create / list /
#                                        select / send / add_task /
#                                        hub_send / pause / resume / stop
#                                        against the real listener (no fake
#                                        backend); parent/child isolation;
#                                        WS drop survival; same-port
#                                        kill/restart handshake writes
#                                        server-up.marker only after the
#                                        respawned listener is ready
#   mermaid_viewer mermaid_viewer.sh           mermaid viewer (playwright-only
#                                        hard gate): always-visible
#                                        View/Download-SVG toolbar on every
#                                        rendered host, ONE aria-modal
#                                        singleton (View/dblclick),
#                                        Fit default transform with
#                                        -/+/Fit/100% + wheel zoom + drag
#                                        pan, Escape/Close focus restore,
#                                        sanitized SVG download (no script/
#                                        foreignObject/embed, no on*
#                                        handlers, no non-local URLs),
#                                        error host has no controls, mobile
#                                        390x844 >=44px touch targets +
#                                        explicit View tap + fit + pan,
#                                        zero console/page errors
#   runtime_matrix runtime_matrix.sh  17-session eviction/resume release matrix
#                                        (PLAYWRIGHT-ONLY, SERIAL): 16-runtime
#                                        cap, exact LRU eviction resume with
#                                        protected busy sessions, cold reopen
#                                        restore, zero-wire switches, bounded RSS/fd/socket
#   session_path_complete session_path_complete.sh
#                                        SessionPanel path completion +
#                                        authenticated catalog: tokened
#                                        catalog row identity, valid workdir
#                                        suggestions quiet, missing in-workspace
#                                        completion fail-closed + exact manual
#                                        new_session failure
#   thinking_level thinking_level.sh   #thinking-select: exactly one
#                                        authoritative set_thinking_level per
#                                        selection, remains selected (idle +
#                                        streaming), per-session levels across
#                                        switches and reload/bootstrap restores
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
#   RPI_WEB_JOBS    max concurrent ordinary lanes (default: min(4, nproc); 1
#                   when RPI_COVERAGE_DIR is set)
set -euo pipefail
SCRIPT_DIR="$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
E2E_DIR="$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd -P)"
# shellcheck source=../lib/common.sh
. "$E2E_DIR/lib/common.sh"
# shellcheck source=lib/fixture.sh
. "$SCRIPT_DIR/lib/fixture.sh"

# verified current-tree bundle, and the once-prepared playwright tree so
# every lane writes under the same roots and no lane independently rebuilds
# the frontend or reinstalls playwright.
export E2E_RUN_ID EVIDENCE_ROOT WORK_ROOT RPI_BIN RPI_WEB_DEV_DIR RPI_WEB_DIST_READY
export RPI_WEB_PLAYWRIGHT_DIR RPI_WEB_PLAYWRIGHT_READY


# Every lane script in the suite (core.sh = the shared core lane).
LANES="core goal xss abort scroll reconnect readygate switch mobile auth auth_tokenless extras sessions sessions_workflow session_restore session_ux sidebar_menu_rename sidebar_rail projects commands_review loop_goal attachments slowclient external_sessions files_latency switch_catalog_identity session_move_delete presentation turn_error panel_chrome realtime_webrtc readygate_spawn code_review_paging code_review_side_chat code_review_reply_lifecycle skill_completion realtime_rpc appborder personas stt_rpc chat chat_mermaid todo_child_session mermaid_viewer runtime_matrix session_path_complete thinking_level code_review_expand code_review_submodule code_review_operations plugin_command processes"
# Coverage collection and pressure/stress/resource-sensitive lanes stay
# serial even when ordinary independent UI lanes overlap. Same-port
# kill/respawn and multi-phase discovery/media lanes are not overlapped.
# files_latency measures wall-clock toggle latency against a 1500-message
# transcript — timing-sensitive, so it always runs serially.
# switch_catalog_identity and session_move_delete hold exact wire-count and
# busy-stream contracts (real model streams with idle gates) — they always
# run serially too.
SERIAL_LANES="sessions sessions_workflow slowclient external_sessions files_latency switch_catalog_identity session_move_delete realtime_webrtc readygate_spawn realtime_rpc stt_rpc chat todo_child_session code_review_side_chat runtime_matrix"

list_lanes() {
    local lane
    for lane in $LANES; do
        bash "$SCRIPT_DIR/$lane.sh" list
    done
}

# Static roster consistency self-check: the README roster block must name
# EXACTLY the LANES registry (same members, same order, no extras, no
# omissions) and the README's stated lane count must equal the actual LANES
# membership. Documentation drift fails the runner (list AND run modes —
# this check runs BEFORE the list/run branch) instead of silently diverging
# from the registry. The standalone exclusions (web-subagents, p0_resource)
# are likewise validated: they must stay documented in the README's Focused
# lanes section and must NEVER appear as LANES members.
roster_self_check() {
    local readme="$SCRIPT_DIR/README.md" documented stated count standalone
    [ -f "$readme" ] || fail "web: roster self-check: $readme is missing"
    documented="$(awk '/^```text$/{f=1;next} /^```$/{f=0} f && NF {print; exit}' "$readme")"
    [ -n "$documented" ] || fail "web: roster self-check: README.md has no \`\`\`text lane-roster block"
    [ "$documented" = "$LANES" ] || fail "web: roster self-check: README lane roster does not match LANES (README: $(printf '%s' "$documented" | cut -c1-120)...; LANES: $(printf '%s' "$LANES" | cut -c1-120)...)"
    stated="$(sed -n 's/.*The suite is \([0-9][0-9]*\) lanes.*/\1/p' "$readme" | head -1)"
    count="$(printf '%s\n' $LANES | wc -l | tr -d ' ')"
    [ "$stated" = "$count" ] || fail "web: roster self-check: README claims $stated lanes but LANES has $count"
    # Standalone (NOT in LANES/run.sh) lanes stay documented as exclusions:
    # web-subagents and p0_resource are outside the suite roster and must be
    # described in the README's Focused lanes section.
    grep -q '^`web-subagents`' "$readme" || fail "web: roster self-check: README does not document the standalone web-subagents exclusion"
    grep -q '^`p0_resource`' "$readme" || fail "web: roster self-check: README does not document the standalone p0_resource exclusion"
    # ...and they must NEVER join the roster: their scripts exist in this
    # directory, so a LANES member named like them would silently run a
    # lane outside the documented roster.
    for standalone in subagents p0_resource; do
        case " $LANES " in
            *" $standalone "*)
                fail "web: roster self-check: standalone exclusion '$standalone' must never be a LANES member"
                ;;
        esac
    done
    log "web: roster self-check OK ($count lanes, README roster matches LANES; standalone web-subagents + p0_resource documented as exclusions)"
}

web_lane_job_limit() {
    if [ -n "${RPI_COVERAGE_DIR:-}" ]; then
        printf '1\n'
        return 0
    fi
    if [ -n "${RPI_WEB_JOBS:-}" ]; then
        n="$RPI_WEB_JOBS"
    elif command -v nproc >/dev/null 2>&1; then
        n="$(nproc)"
        [ "$n" -gt 4 ] && n=4
    else
        n=2
    fi
    [ "$n" -ge 1 ] || n=1
    printf '%s\n' "$n"
}

web_lane_work() {
    local name="$1" dir="$WORK_ROOT/$name"
    [ "$name" = "todo_child_session" ] && dir="$WORK_ROOT/web-todo_child_session"
    [ "$name" = "sessions_workflow" ] && dir="$WORK_ROOT/web-sessions-workflow"
    [ "$name" = "session_ux" ] && dir="$WORK_ROOT/web-session_ux"
    [ "$name" = "switch_catalog_identity" ] && dir="$WORK_ROOT/web-switch-catalog-identity"
    [ "$name" = "session_move_delete" ] && dir="$WORK_ROOT/web-session-move-delete"
    [ "$name" = "mermaid_viewer" ] && dir="$WORK_ROOT/web-mermaid-viewer"
    [ "$name" = "code_review_reply_lifecycle" ] && dir="$WORK_ROOT/web-code-review-reply-lifecycle"
    [ "$name" = "files_latency" ] && dir="$WORK_ROOT/web-files-latency"
    [ "$name" = "external_sessions" ] && dir="$WORK_ROOT/web-external_sessions"
    [ "$name" = "chat_mermaid" ] && dir="$WORK_ROOT/web-chat-mermaid"
    [ "$name" = "code_review_side_chat" ] && dir="$WORK_ROOT/web-code-review-side-chat"
    [ "$name" = "sidebar_menu_rename" ] && dir="$WORK_ROOT/web-sidebar-menu-rename"
    [ "$name" = "sidebar_rail" ] && dir="$WORK_ROOT/web-sidebar-rail"
    printf '%s\n' "$dir"
}

web_lane_evidence() {
    local name="$1" dir="$EVIDENCE_ROOT/web-$name"
    [ "$name" = "core" ] && dir="$EVIDENCE_ROOT/web"
    [ "$name" = "sessions_workflow" ] && dir="$EVIDENCE_ROOT/web-sessions-workflow"
    [ "$name" = "mermaid_viewer" ] && dir="$EVIDENCE_ROOT/web-mermaid-viewer"
    [ "$name" = "code_review_reply_lifecycle" ] && dir="$EVIDENCE_ROOT/web-code-review-reply-lifecycle"
    [ "$name" = "files_latency" ] && dir="$EVIDENCE_ROOT/web-files-latency"
    [ "$name" = "switch_catalog_identity" ] && dir="$EVIDENCE_ROOT/web-switch-catalog-identity"
    [ "$name" = "session_move_delete" ] && dir="$EVIDENCE_ROOT/web-session-move-delete"
    [ "$name" = "chat_mermaid" ] && dir="$EVIDENCE_ROOT/web-chat-mermaid"
    [ "$name" = "code_review_side_chat" ] && dir="$EVIDENCE_ROOT/web-code-review-side-chat"
    [ "$name" = "sidebar_menu_rename" ] && dir="$EVIDENCE_ROOT/web-sidebar-menu-rename"
    [ "$name" = "sidebar_rail" ] && dir="$EVIDENCE_ROOT/web-sidebar-rail"
    printf '%s\n' "$dir"
}

web_lane_is_serial() {
    local name="$1" serial
    [ -n "${RPI_COVERAGE_DIR:-}" ] && return 0
    for serial in $SERIAL_LANES; do
        [ "$serial" = "$name" ] && return 0
    done
    return 1
}

web_record_job() {
    local pid="$1" report="$2" status="$3" lane start lane_evidence
    lane="${WEB_JOB_LANE[$pid]}"
    start="${WEB_JOB_START[$pid]}"
    lane_evidence="${WEB_JOB_EVIDENCE[$pid]}"
    unset "WEB_JOB_LANE[$pid]" "WEB_JOB_START[$pid]" "WEB_JOB_EVIDENCE[$pid]"
    WEB_JOB_RUNNING=$((WEB_JOB_RUNNING - 1))
    if [ "$status" -eq 0 ]; then
        web_lane_report "$report" "$lane" "PASS" "$lane_evidence"
        log "web: lane '$lane' PASSED ($((SECONDS - start))s)"
        return 0
    fi
    web_lane_report "$report" "$lane" "FAIL" "$lane_evidence"
    log "web: lane '$lane' FAILED (exit $status, $((SECONDS - start))s)"
    return 1
}

web_wait_one_job() {
    local report="$1" pid status=0
    [ "$WEB_JOB_RUNNING" -gt 0 ] || return 0
    for pid in "${!WEB_JOB_LANE[@]}"; do
        if ! kill -0 "$pid" 2>/dev/null; then
            wait "$pid" || status=$?
            web_record_job "$pid" "$report" "$status"
            return $?
        fi
    done
    wait -n -p pid "${!WEB_JOB_LANE[@]}" || status=$?
    [ -n "${pid:-}" ] && [ -n "${WEB_JOB_LANE[$pid]+x}" ] || return 0
    web_record_job "$pid" "$report" "$status"
}

web_wait_all_jobs() {
    local report="$1" overall=0
    while [ "$WEB_JOB_RUNNING" -gt 0 ]; do
        web_wait_one_job "$report" || overall=1
    done
    return "$overall"
}

web_start_lane() {
    local name="$1" lane_work
    lane_work="$(web_lane_work "$name")"
    log "web: lane '$name' starting"
    E2E_CLEANUP_PATHS="$lane_work" bash "$SCRIPT_DIR/$name.sh" run &
    WEB_JOB_LANE[$!]="$name"
    WEB_JOB_START[$!]="$SECONDS"
    WEB_JOB_EVIDENCE[$!]="$(web_lane_evidence "$name")"
    WEB_JOB_RUNNING=$((WEB_JOB_RUNNING + 1))
}

main() {
    # Roster/documentation consistency gates EVERY mode: list, run, and any
    # subset run all fail on a drifted README instead of printing a stale
    # roster.
    roster_self_check

    case "${1:-run}" in
        list|--list|--dry-run)
            printf '%s\n' '[Web client suite (playwright-only hard gate)]'
            list_lanes
            return 0
            ;;
        run|all) ;;
        *) fail "usage: $0 [run|list]" ;;
    esac

    # Subset tokens must be EXACT members of the LANES registry, checked
    # BEFORE any fixture/browser setup: the standalone exclusions
    # (web-subagents, p0_resource) ship scripts in this directory but are
    # deliberately OUTSIDE the roster, so a subset naming them (or any
    # unknown lane) is a hard error — never a silent run of an unrostered
    # script.
    local lanes="${RPI_WEB_LANES:-$LANES}"
    local token
    for token in $lanes; do
        case " $LANES " in
            *" $token "*) ;;
            *) fail "web: RPI_WEB_LANES token '$token' is not an exact LANES member (standalone exclusions web-subagents/p0_resource are not runnable via RPI_WEB_LANES)" ;;
        esac
    done

    require_rpi
    # The web release lanes are playwright-only hard gates: node is required
    # up front (fail fast), and the suite verifies a usable Chromium path
    # once after the shared playwright install. Direct lane invocation still
    # prepares playwright in the lane work dir.
    web_require_browser
    web_prepare_dist "$WORK_ROOT/web-dist"
    web_prepare_playwright "$WORK_ROOT/playwright"

    local report="$EVIDENCE_ROOT/web/REPORT.md"
    mkdir -p "$EVIDENCE_ROOT/web"
    printf '%s\n' '# Web client E2E suite report' '' '| lane | result | evidence |' '|---|---|---|' >"$report"

    local lane overall=0 max_jobs
    max_jobs="$(web_lane_job_limit)"
    WEB_JOB_RUNNING=0
    declare -gA WEB_JOB_LANE=() WEB_JOB_START=() WEB_JOB_EVIDENCE=()

    for lane in $lanes; do
        # Every listed lane must reach its playwright .mjs or fail: an
        # unknown lane name is a hard error, not a skip.
        [ -f "$SCRIPT_DIR/$lane.sh" ] || fail "web: unknown lane '$lane' (no $SCRIPT_DIR/$lane.sh)"
        if web_lane_is_serial "$lane"; then
            web_wait_all_jobs "$report" || overall=1
            web_start_lane "$lane"
            web_wait_all_jobs "$report" || overall=1
            continue
        fi
        while [ "$WEB_JOB_RUNNING" -ge "$max_jobs" ]; do
            web_wait_one_job "$report" || overall=1
        done
        web_start_lane "$lane"
    done
    web_wait_all_jobs "$report" || overall=1

    web_summary "$report" || overall=1
    if [ "$overall" -eq 0 ]; then
        log "web: full suite PASSED (report: $report)"
    else
        log "web: full suite has FAILURES (report: $report)"
    fi
    exit "$overall"
}
main "$@"
