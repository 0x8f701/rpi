#!/usr/bin/env bash
# User-perspective /btw side chat: open the overlay, submit a parallel prompt,
# close with Esc (session kept), reopen to prove persistence, then create,
# list, switch, and close named tabs.
#
# Runs the real rpi TUI in an isolated tmux session with the faux provider.
set -euo pipefail
SCRIPT_DIR="$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
E2E_DIR="$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd -P)"
# shellcheck source=../lib/common.sh
. "$E2E_DIR/lib/common.sh"
# shellcheck source=../lib/orchestration_fixtures.sh
. "$E2E_DIR/lib/orchestration_fixtures.sh"

SCENARIO="btw-side-chat"
FAUX_REPLY="deterministic-btw-reply"

wait_for() { # needle [timeout]
    local needle="$1" timeout="${2:-25}"
    if ! tmux_wait_for "$session" "$timeout" "$needle" >"$evidence/live.txt"; then
        fail "user.$SCENARIO: did not find ${needle@Q} (see $evidence/live.txt)"
    fi
}

send_cmd() { # literal command line
    tmux send-keys -t "$session":0 -l "$1"
    tmux send-keys -t "$session":0 Enter
}

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
'$RPI_BIN' --offline --model faux/faux-1; printf '===TUI-DONE-btw-side-chat===\n'"

    tmux_wait_for "$session" 25 'faux/faux-1' >"$evidence/boot.txt" || true
    # Dismiss the startup resume selector when it is showing (harmless otherwise).
    tmux send-keys -t "$session":0 Escape
    sleep 0.3

    # --- Main-session anchor turn first ---
    send_cmd 'main session anchor'
    wait_for "$FAUX_REPLY" 20
    sleep 0.5

    # --- /btw opens the overlay on the default tab ---
    send_cmd '/btw'
    wait_for 'Side chat · default' 15
    wait_for 'read-only' 15
    wait_for 'Ctrl+T' 15
    wait_for '▸default' 15

    # --- Type directly into the side editor and submit (Enter) ---
    tmux send-keys -t "$session":0 -l 'side-chat-probe-prompt'
    sleep 0.3
    tmux send-keys -t "$session":0 Enter
    wait_for "$FAUX_REPLY" 25

    # --- Esc closes the overlay; the composer accepts input again ---
    tmux send-keys -t "$session":0 Escape
    sleep 0.4
    assert_tmux_composer_editable "$session" "$evidence/composer.txt" 'BTW-MAIN-SENTINEL'

    # --- Reopen: the side conversation persists (session kept) ---
    send_cmd '/btw'
    wait_for 'Side chat · default' 15
    wait_for 'side-chat-probe-prompt' 15
    tmux send-keys -t "$session":0 Escape
    sleep 0.4

    # --- Named tabs (typed in the main composer with the overlay closed) ---
    send_cmd '/btw new alpha'
    wait_for 'Side chat · alpha' 20
    wait_for 'tabs: default · ▸alpha' 20
    tmux send-keys -t "$session":0 -l 'alpha-tab-probe'
    sleep 0.3
    tmux send-keys -t "$session":0 Enter
    wait_for "$FAUX_REPLY" 25
    tmux send-keys -t "$session":0 Escape
    sleep 0.4
    send_cmd '/btw list'
    wait_for 'Side tabs (' 15
    send_cmd '/btw alpha'
    wait_for 'Side chat · alpha' 15
    wait_for 'alpha-tab-probe' 15
    tmux send-keys -t "$session":0 Escape
    sleep 0.4
    send_cmd '/btw close alpha'
    wait_for 'Closed side tab alpha ·' 15
    wait_for 'tabs open' 15

    # --- Close the overlay, verify the main composer again, quit cleanly ---
    tmux send-keys -t "$session":0 Escape
    sleep 0.4
    assert_tmux_composer_editable "$session" "$evidence/composer-2.txt" 'BTW-MAIN-SENTINEL-2'

    tmux capture-pane -p -S -3000 -t "$session":0 >"$evidence/tui.txt" 2>/dev/null || true
    tmux send-keys -t "$session":0 -l '/quit'
    tmux send-keys -t "$session":0 Enter
    if ! tmux_wait_for "$session" 12 '===TUI-DONE-btw-side-chat===' >"$evidence/quit.txt"; then
        log "user.$SCENARIO: quit marker not seen; scenario assertions already passed, tearing down"
    fi
    tmux kill-session -t "$session" 2>/dev/null || true

    # Side-chat content and statuses live in the overlay/status rows (repainted,
    # never scrolled); the per-step waits above are the assertions.
    printf 'user.%s passed\nevidence=%s\n' "$SCENARIO" "$evidence"
}

main "$@"
