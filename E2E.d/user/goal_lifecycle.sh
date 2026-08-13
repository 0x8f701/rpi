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

main() {
    require_rpi
    require_cmd tmux

    local root evidence session
    root="$(scenario_workspace "$SCENARIO")"
    evidence="$EVIDENCE_ROOT/$SCENARIO"
    session="$(unique_tmux_name "$SCENARIO")"
    register_tmux_session "$session"

    tmux new-session -d -s "$session" -x 140 -y 42 -c "$root/workspace" \
        "env HOME='$root/home' USERPROFILE='$root/home' \
PI_CODING_AGENT_DIR='$root/home/.pi/agent' \
PI_OFFLINE=1 PI_SKIP_VERSION_CHECK=1 \
PI_FAUX_RESPONSE='$FAUX_REPLY' \
TERM=xterm-256color \
'$RPI_BIN' --offline --model faux/faux-1; printf '===TUI-DONE-goal-lifecycle===\n'"

    tmux_wait_for "$session" 25 'faux/faux-1' >"$evidence/boot.txt" || true
    # Dismiss the startup resume selector when it is showing (harmless otherwise).
    tmux send-keys -t "$session":0 Escape
    sleep 0.3

    # --- Create with a token budget ---
    tmux send-keys -t "$session":0 -l '/goal create --tokens 100 ship the widget'
    tmux send-keys -t "$session":0 Enter
    # Details block in the live transcript (the transient "Goal work started"
    # status is immediately superseded by the turn's Working label).
    tmux_wait_for "$session" 25 'Objective: ship the widget' >"$evidence/create-details.txt" \
        || fail "goal details block missing Objective line"
    tmux_wait_for "$session" 15 'Status: active' >/dev/null \
        || fail "goal details block missing Status: active"
    tmux_wait_for "$session" 15 'Tokens: 0 / 100' >/dev/null \
        || fail "goal details block missing Tokens: 0 / 100"
    tmux_wait_for "$session" 15 '🎯 Goal' >"$evidence/create-chip.txt" \
        || fail "composer header goal chip missing (expected 🎯 Goal lifecycle marker)"
    # The goal work turn answered with the faux reply.
    tmux_wait_for "$session" 15 "$FAUX_REPLY" >/dev/null || true

    # --- Inspect ---
    tmux send-keys -t "$session":0 -l '/goal show'
    tmux send-keys -t "$session":0 Enter
    tmux_wait_for "$session" 15 'Status: active' >"$evidence/show.txt" \
        || fail "/goal show details block missing Status: active"
    tmux_wait_for "$session" 15 'Tokens: 0 / 100' >/dev/null \
        || fail "/goal show details block missing Tokens: 0 / 100"

    # --- Pin + pins ---
    tmux send-keys -t "$session":0 -l '/goal pin keep the release checklist in scope'
    tmux send-keys -t "$session":0 Enter
    tmux_wait_for "$session" 15 'active · 0/100 tokens · ship the widget' >/dev/null \
        || fail "goal pin did not keep the active summary"
    tmux send-keys -t "$session":0 -l '/goal pins'
    tmux send-keys -t "$session":0 Enter
    tmux_wait_for "$session" 15 '1. keep the release checklist in scope' >"$evidence/pins.txt" \
        || fail "/goal pins did not list the pinned text"
    # Pins must also surface inside the /goal show details block.
    tmux send-keys -t "$session":0 -l '/goal show'
    tmux send-keys -t "$session":0 Enter
    tmux_wait_for "$session" 15 'Pins:' >/dev/null \
        || fail "/goal show details block missing Pins: section"

    # --- Pause ---
    tmux send-keys -t "$session":0 -l '/goal pause'
    tmux send-keys -t "$session":0 Enter
    tmux_wait_for "$session" 15 'paused (manually paused) · 0/100 tokens · ship the widget' >"$evidence/pause.txt" \
        || fail "goal pause did not report 'paused (manually paused) · 0/100 tokens · ship the widget'"
    tmux_wait_for "$session" 15 '⏸ Goal' >/dev/null \
        || fail "paused goal chip missing (expected ⏸ Goal lifecycle marker)"

    # --- Resume ---
    tmux send-keys -t "$session":0 -l '/goal resume'
    tmux send-keys -t "$session":0 Enter
    # Resume restarts goal work (a turn starts, so the status line shows the
    # busy label); the observable proof is the chip returning to active.
    tmux_wait_for "$session" 25 '🎯 Goal' >"$evidence/resume.txt" \
        || fail "resumed goal chip missing (expected 🎯 Goal lifecycle marker)"
    tmux send-keys -t "$session":0 -l '/goal show'
    tmux send-keys -t "$session":0 Enter
    tmux_wait_for "$session" 15 'Status: active' >/dev/null \
        || fail "resumed goal details block missing Status: active"

    # --- Complete ---
    tmux send-keys -t "$session":0 -l '/goal complete'
    tmux send-keys -t "$session":0 Enter
    tmux_wait_for "$session" 15 'completed · 0/100 tokens · ship the widget' >"$evidence/complete.txt" \
        || fail "goal complete did not report 'completed · 0/100 tokens · ship the widget'"
    tmux_wait_for "$session" 15 '✓ Goal' >/dev/null \
        || fail "completed goal chip missing (expected ✓ Goal lifecycle marker)"

    # --- Bare /goal opens the goal panel; Esc restores the composer ---
    tmux send-keys -t "$session":0 -l '/goal'
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

    tmux kill-session -t "$session" 2>/dev/null || true
    tmux kill-session -t "$session" 2>/dev/null || true
    assert_file_contains "$evidence/tui.txt" \
        'Objective: ship the widget' \
        'Status: active' \
        'keep the release checklist in scope'
    printf 'user.%s passed\nevidence=%s\n' "$SCENARIO" "$evidence"
}

main "$@"
