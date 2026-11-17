#!/usr/bin/env bash
# User-perspective goal lifecycle: create (with token budget) -> pin -> pause
# -> resume -> complete, plus the header chip and details block.
#
# Runs the real rpi TUI in an isolated tmux session with the faux provider.
# Asserts the OMP-style status line, the composer-header goal chip, and the
# transcript details block after every transition.
set -euo pipefail
SCRIPT_DIR="$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
E2E_DIR="$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd -P)"
# shellcheck source=../lib/common.sh
. "$E2E_DIR/lib/common.sh"
# shellcheck source=../lib/orchestration_fixtures.sh
. "$E2E_DIR/lib/orchestration_fixtures.sh"

SCENARIO="goal-lifecycle"
FAUX_REPLY="deterministic-goal-reply"
PIN_TEXT="keep the release checklist in scope"
GOAL_TMUX_SOCKET="rpi-e2e-goal-lifecycle-$$"

# Local socket only. common.sh tmux helpers talk to the default server;
# shadowing tmux here keeps this lane off that server without editing them.
tmux() {
    command tmux -L "$GOAL_TMUX_SOCKET" -f /dev/null "$@"
}

cleanup_goal_lifecycle() {
    command tmux -L "$GOAL_TMUX_SOCKET" -f /dev/null kill-server >/dev/null 2>&1 || true
    cleanup_e2e
}
trap cleanup_goal_lifecycle EXIT
trap 'trap - EXIT HUP INT TERM; cleanup_goal_lifecycle; exit 129' HUP
trap 'trap - EXIT HUP INT TERM; cleanup_goal_lifecycle; exit 130' INT
trap 'trap - EXIT HUP INT TERM; cleanup_goal_lifecycle; exit 143' TERM

capture_goal_pane() {
    local session="$1"
    tmux capture-pane -p -S -3000 -t "$session":0 || fail "tmux capture failed"
}

tmux_submit_composer() {
    local session="$1" line="$2" label="$3" expected="${4:-}" deadline capture
    tmux send-keys -t "$session":0 -l "$line"
    tmux_wait_for "$session" 10 "$line" >/dev/null \
        || fail "$label: composer never showed the command"
    tmux send-keys -t "$session":0 Enter
    deadline=$((SECONDS + 10))
    while ((SECONDS < deadline)); do
        capture="$(capture_goal_pane "$session" 2>/dev/null || true)"
        case "$capture" in
            *"$line"*) sleep 0.25; continue ;;
        esac
        if [ -n "$expected" ]; then
            case "$capture" in
                *"$expected"*) printf '%s\n' "$capture"; return 0 ;;
                *) sleep 0.25; continue ;;
            esac
        fi
        printf '%s\n' "$capture"
        return 0
    done
    if [ -n "$expected" ]; then
        fail "$label: status ${expected@Q} missing from the submit frame"
    fi
    fail "$label: command stayed in the composer (Enter did not submit)"
}

wait_new_goal_block() {
    local session="$1" before="$2" timeout="$3" needle="$4" evidence="$5"
    local deadline capture count=0 block=''
    deadline=$((SECONDS + timeout))
    while ((SECONDS < deadline)); do
        capture="$(capture_goal_pane "$session")"
        count="$(printf '%s\n' "$capture" | grep -c -F 'Objective:' || true)"
        if [ "$count" -gt "$before" ]; then
            block="$(printf '%s\n' "$capture" | awk -v n="$count" '
                /Objective:/ { seen++; buf = $0; next }
                seen == n { buf = buf ORS $0 }
                END { print buf }
            ')"
            printf '%s\n' "$block" >"$evidence"
            case "$block" in
                *"$needle"*) return 0 ;;
            esac
            fail "$evidence: newest Goal block missing ${needle@Q}"
        fi
        sleep 0.25
    done
    capture_goal_pane "$session" >"$evidence" || true
    fail "$evidence: Goal block count did not grow past $before"
}

wait_new_pin_row() {
    local session="$1" before="$2" timeout="$3" needle="$4" evidence="$5"
    local deadline capture count=0
    deadline=$((SECONDS + timeout))
    while ((SECONDS < deadline)); do
        capture="$(capture_goal_pane "$session")"
        count="$(printf '%s\n' "$capture" | grep -c -F "$needle" || true)"
        if [ "$count" -gt "$before" ]; then
            printf '%s\n' "$capture" >"$evidence"
            return 0
        fi
        sleep 0.25
    done
    capture_goal_pane "$session" >"$evidence" || true
    fail "$evidence: pin row count did not grow past $before"
}

count_goal_blocks() {
    capture_goal_pane "$1" | grep -c -F 'Objective:' || true
}

count_faux_replies() {
    capture_goal_pane "$1" | grep -c -F "$FAUX_REPLY" || true
}

main() {
    require_rpi
    require_cmd tmux

    local root evidence session replies_before_resume replies_after_resume blocks_before pins_before deadline capture
    root="$(scenario_workspace "$SCENARIO")"
    evidence="$EVIDENCE_ROOT/$SCENARIO"
    session="$(unique_tmux_name "$SCENARIO")"
    register_tmux_session "$session"

    tmux new-session -d -s "$session" -x 140 -y 42 -c "$root/workspace" \
        "env HOME='$root/home' USERPROFILE='$root/home' \
PI_CODING_AGENT_DIR='$root/home/.pi/agent' \
RPI_OFFLINE=1 RPI_SKIP_VERSION_CHECK=1 \
PI_FAUX_RESPONSE='$FAUX_REPLY' \
TERM=xterm-256color \
'$RPI_BIN' --offline --model faux/faux-1; printf '===TUI-DONE-goal-lifecycle===\n'"

    tmux_wait_for "$session" 25 'faux/faux-1' >"$evidence/boot.txt" || true
    # Dismiss the startup resume selector when it is showing (harmless otherwise).
    tmux send-keys -t "$session":0 Escape
    sleep 0.3

    # --- Create, then pause the auto-continuing work turn ---
    # Faux replies carry zero usage, and a successful stop now continues an
    # active goal. Pause as soon as the first reply lands so the lifecycle
    # commands are not racing an unbounded worker. The create bubble is
    # superseded by the turn, so the durable proof is the details block.
    tmux_submit_composer "$session" '/goal create --tokens 100 ship the widget' 'goal create'
    tmux_wait_for "$session" 25 'Objective: ship the widget' >"$evidence/create-details.txt" \
        || fail "goal details block missing Objective line"
    tmux_wait_for "$session" 15 'Status: active' >/dev/null \
        || fail "goal details block missing Status: active"
    tmux_wait_for "$session" 15 'Tokens: 0 / 100' >/dev/null \
        || fail "goal details block missing Tokens: 0 / 100"
    tmux_wait_for "$session" 15 '🎯 Goal' >"$evidence/create-chip.txt" \
        || fail "composer header goal chip missing (expected 🎯 Goal lifecycle marker)"
    tmux_wait_for "$session" 15 "$FAUX_REPLY" >/dev/null \
        || fail "goal work turn did not answer with the faux reply"
    capture="$(tmux_submit_composer "$session" '/goal pause' 'goal pause after create' 'paused (manually paused) · 0/100 tokens · ship the widget')"
    printf '%s\n' "$capture" >"$evidence/pause.txt"
    tmux_wait_for "$session" 15 '⏸ Goal' >/dev/null \
        || fail "paused goal chip missing (expected ⏸ Goal lifecycle marker)"
    deadline=$((SECONDS + 15))
    while ((SECONDS < deadline)); do
        capture="$(capture_goal_pane "$session")"
        case "$capture" in
            *'Working'*) sleep 0.25; continue ;;
        esac
        replies_before_resume="$(count_faux_replies "$session")"
        printf '%s\n' "$capture" >"$evidence/paused-settled.txt"
        break
    done
    case "$capture" in
        *'Working'*) fail "paused goal never left Working before the resume baseline" ;;
    esac

    blocks_before="$(count_goal_blocks "$session")"
    tmux_submit_composer "$session" '/goal show' 'goal show' >/dev/null
    wait_new_goal_block "$session" "$blocks_before" 15 'Status: paused (manually paused)' "$evidence/show.txt"
    grep -F 'Tokens: 0 / 100' "$evidence/show.txt" >/dev/null \
        || fail "/goal show details block missing Tokens: 0 / 100"

    capture="$(tmux_submit_composer "$session" "/goal pin $PIN_TEXT" 'goal pin' 'paused (manually paused) · 0/100 tokens · ship the widget')"
    printf '%s\n' "$capture" >"$evidence/pin.txt"
    pins_before="$(printf '%s\n' "$capture" | grep -c -F "1. $PIN_TEXT" || true)"
    tmux_submit_composer "$session" '/goal pins' 'goal pins' >/dev/null
    wait_new_pin_row "$session" "$pins_before" 15 "1. $PIN_TEXT" "$evidence/pins.txt"
    blocks_before="$(count_goal_blocks "$session")"
    tmux_submit_composer "$session" '/goal show' 'goal show after pins' >/dev/null
    wait_new_goal_block "$session" "$blocks_before" 15 'Pins:' "$evidence/show-pins.txt"
    grep -F 'Status: paused (manually paused)' "$evidence/show-pins.txt" >/dev/null \
        || fail "/goal show after pins is not the current paused goal"

    tmux_submit_composer "$session" '/goal resume' 'goal resume' >/dev/null
    tmux_wait_for "$session" 25 '🎯 Goal' >"$evidence/resume.txt" \
        || fail "resumed goal chip missing (expected 🎯 Goal lifecycle marker)"
    deadline=$((SECONDS + 20))
    replies_after_resume="$replies_before_resume"
    while ((SECONDS < deadline)); do
        replies_after_resume="$(count_faux_replies "$session")"
        if [ "$replies_after_resume" -gt "$replies_before_resume" ]; then
            break
        fi
        sleep 0.25
    done
    [ "$replies_after_resume" -gt "$replies_before_resume" ] \
        || fail "resume did not add a faux reply (before=$replies_before_resume after=$replies_after_resume)"
    capture="$(tmux_submit_composer "$session" '/goal complete' 'goal complete' 'completed · 0/100 tokens · ship the widget')"
    printf '%s\n' "$capture" >"$evidence/complete.txt"
    tmux_wait_for "$session" 15 '✓ Goal' >/dev/null \
        || fail "completed goal chip missing (expected ✓ Goal lifecycle marker)"
    blocks_before="$(count_goal_blocks "$session")"
    tmux_submit_composer "$session" '/goal show' 'goal show after complete' >/dev/null
    wait_new_goal_block "$session" "$blocks_before" 15 'Status: completed' "$evidence/show-complete.txt"

    # --- Bare /goal opens the goal panel; Esc restores the composer ---
    tmux send-keys -t "$session":0 -l '/goal'
    tmux_wait_for "$session" 10 '/goal' >/dev/null || fail "bare /goal: composer never showed the command"
    tmux send-keys -t "$session":0 Enter
    tmux_wait_for "$session" 15 'Show details' >"$evidence/panel.txt" \
        || fail "bare /goal did not open the goal panel with 'Show details'"
    tmux send-keys -t "$session":0 Escape
    sleep 0.4
    assert_tmux_composer_editable "$session" "$evidence/composer.txt" 'GOAL-SENTINEL'

    # --- Clean quit proves terminal restoration (best-effort teardown) ---
    tmux capture-pane -p -S -3000 -t "$session":0 >"$evidence/tui.txt" 2>/dev/null || true
    tmux send-keys -t "$session":0 -l '/quit'
    tmux send-keys -t "$session":0 Enter
    if ! tmux_wait_for "$session" 12 '===TUI-DONE-goal-lifecycle===' >"$evidence/quit.txt"; then
        log "user.$SCENARIO: quit marker not seen; scenario assertions already passed, tearing down"
    fi

    command tmux -L "$GOAL_TMUX_SOCKET" -f /dev/null kill-server >/dev/null 2>&1 || true
    assert_file_contains "$evidence/tui.txt" \
        'Objective: ship the widget' \
        'Status: active' \
        'keep the release checklist in scope'
    printf 'user.%s passed\nevidence=%s\n' "$SCENARIO" "$evidence"
}

main "$@"
