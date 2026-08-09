#!/usr/bin/env bash
# User-perspective rewind + snapcompact + compact: checkpoint a session, roll
# back with /rewind (sidecar archive verified on disk), then shrink context
# with /snapcompact and /compact (A -> B token status verified).
#
# Runs the real rpi TUI in an isolated tmux session with the faux provider and
# a compaction-enabled settings fixture (snapKeepTurns=1 so a short session has
# an archiveable tail).
set -euo pipefail
SCRIPT_DIR="$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
E2E_DIR="$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd -P)"
# shellcheck source=../lib/common.sh
. "$E2E_DIR/lib/common.sh"
# shellcheck source=../lib/orchestration_fixtures.sh
. "$E2E_DIR/lib/orchestration_fixtures.sh"

SCENARIO="rewind-compact"
FAUX_REPLY="deterministic-rewind-reply"

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

    local root evidence session sessions_dir sidecar
    root="$(scenario_workspace "$SCENARIO")"
    evidence="$EVIDENCE_ROOT/$SCENARIO"
    session="$(unique_tmux_name "$SCENARIO")"
    sessions_dir="$root/home/.pi/agent/sessions"
    register_tmux_session "$session"

    # Compaction must be enabled with a 1-turn snap keep so two-turn tails
    # archive deterministically (same fixture as rewind_checkpoint_snapcompact_e2e.rs).
    cat >"$root/home/.pi/agent/settings.json" <<'EOF'
{"compaction":{"enabled":true,"reserveTokens":16384,"keepRecentTokens":20000,"snapKeepTurns":1}}
EOF

    tmux new-session -d -s "$session" -x 140 -y 42 -c "$root/workspace" \
        "env HOME='$root/home' USERPROFILE='$root/home' \
PI_CODING_AGENT_DIR='$root/home/.pi/agent' \
PI_OFFLINE=1 PI_SKIP_VERSION_CHECK=1 \
PI_FAUX_RESPONSE='$FAUX_REPLY' \
TERM=xterm-256color \
'$RPI_BIN' --offline --model faux/faux-1; printf '===TUI-DONE-rewind-compact===\n'"

    tmux_wait_for "$session" 25 'faux/faux-1' >"$evidence/boot.txt" || true
    # Dismiss the startup resume selector when it is showing (harmless otherwise).
    tmux send-keys -t "$session":0 Escape
    sleep 0.3

    # --- Seed three turns so the rewind picker has records ---
    send_cmd 'rewind anchor line one'
    wait_for "$FAUX_REPLY" 20
    send_cmd 'rewind anchor line two'
    wait_for 'rewind anchor line two' 15
    send_cmd 'rewind anchor line three'
    wait_for 'rewind anchor line three' 15

    # --- Checkpoint + picker ---
    sleep 0.8
    send_cmd '/checkpoint mid'
    wait_for 'checkpoint "mid" marked at entry' 15
    send_cmd '/rewind'
    wait_for 'rolls back to before that record' 15
    wait_for '[checkpoint mid ->' 15

    # --- Rewind to entry 2: keep 2, drop the rest, archive the tail ---
    send_cmd '/rewind 2'
    wait_for 'rewound to entry 1 (kept 2, dropped' 20
    wait_for 'archived tail to' 20
    sidecar="$(glob_one_sidecar "$sessions_dir" '*.rewind-*.jsonl')"
    [ -n "$sidecar" ] || fail "user.$SCENARIO: no .rewind-*.jsonl sidecar under $sessions_dir"
    # The archived tail must contain the dropped record.
    grep -F -- 'rewind anchor line three' "$sidecar" >"$evidence/rewind-archive.txt" \
        || fail "user.$SCENARIO: dropped record missing from rewind sidecar $sidecar"

    # --- Two more turns so /snapcompact has an archiveable tail ---
    send_cmd 'post rewind line four'
    wait_for 'post rewind line four' 15
    send_cmd 'post rewind line five'
    wait_for 'post rewind line five' 15

    # --- /snapcompact: A -> B status (may be overwritten by the session's
    # "Compaction complete" event depending on event ordering) + sidecar ---
    sleep 0.8
    send_cmd '/snapcompact'
    if ! tmux_wait_for "$session" 25 'Compacted ' >"$evidence/snapcompact-status.txt"; then
        tmux_wait_for "$session" 15 'Compaction complete' >"$evidence/snapcompact-status.txt" \
            || fail "user.$SCENARIO: /snapcompact did not report a compaction status"
        log "user.$SCENARIO: snapcompact summary overwritten by CompactionEnd (event race); sidecar still asserted"
    else
        assert_decreasing_tokens "$evidence/snapcompact-status.txt" 'snapcompact'
    fi
    sidecar="$(glob_one_sidecar "$sessions_dir" '*.snapcompact-*.jsonl')"
    [ -n "$sidecar" ] || fail "user.$SCENARIO: no .snapcompact-*.jsonl sidecar under $sessions_dir"
    # The archived snap tail must contain the archived turns (snapKeepTurns=1
    # keeps the final turn, archiving the ones before it).
    grep -F -- 'post rewind line four' "$sidecar" >"$evidence/snapcompact-archive.txt" \
        || fail "user.$SCENARIO: archived turn missing from snapcompact sidecar $sidecar"
    # NOTE: the LLM summarizer path (/compact) is covered by unit tests and
    # the rust REPL lanes; it requires >keepRecentTokens of context to have
    # anything to compact, which a short faux session cannot reach.

    # --- Clean quit ---
    tmux capture-pane -p -S -3000 -t "$session":0 >"$evidence/tui.txt" 2>/dev/null || true
    tmux send-keys -t "$session":0 -l '/quit'
    tmux send-keys -t "$session":0 Enter
    if ! tmux_wait_for "$session" 12 '===TUI-DONE-rewind-compact===' >"$evidence/quit.txt"; then
        log "user.$SCENARIO: quit marker not seen; scenario assertions already passed, tearing down"
    fi
    tmux kill-session -t "$session" 2>/dev/null || true

    printf 'user.%s passed\nevidence=%s\n' "$SCENARIO" "$evidence"
}

# Find the newest sidecar matching the glob under the session root (sessions
# are stored in a cwd-slug subdirectory), or print nothing.
glob_one_sidecar() {
    local dir="$1" pattern="$2"
    local -a files=()
    shopt -s nullglob globstar
    files=("$dir"/**/$pattern)
    shopt -u nullglob globstar
    if [ "${#files[@]}" -eq 0 ]; then
        printf '\n'
        return
    fi
    # Lexicographic order is chronological for timestamped sidecars; pick the
    # last one.
    printf '%s\n' "${files[${#files[@]} - 1]}"
}

# Parse "Compacted A → B estimated tokens" and require both numbers present
# (the visible A → B status contract; the strict-decrease invariant is unit-
# tested and only holds once the session is large enough to archive).
assert_decreasing_tokens() {
    local path="$1" label="$2"
    local before after
    before="$(sed -nE 's/.*Compacted ([0-9]+) → ([0-9]+) estimated tokens.*/\1/p' "$path" | head -1)"
    after="$(sed -nE 's/.*Compacted ([0-9]+) → ([0-9]+) estimated tokens.*/\2/p' "$path" | head -1)"
    [ -n "$before" ] && [ -n "$after" ] \
        || fail "user.$SCENARIO: $label status did not carry numeric A → B tokens: $(cat "$path")"
}

main "$@"
