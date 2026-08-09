#!/usr/bin/env bash
# User-perspective steering queue + /handoff: submit a prompt while the mock
# streams slowly, type a follow-up mid-turn (it must queue: ⚙ header count +
# "Queued follow-up"), watch it drain automatically when the turn completes,
# verify /queue reports empty, then exercise /queue cancel idempotency and the
# deterministic /handoff envelope (captured through a fake xclip seam).
#
# The mock provider (E2E.d/lib/user_mock_server.py) streams the first request
# slowly so the queue window is deterministic; every odd request is slow.
set -euo pipefail
SCRIPT_DIR="$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
E2E_DIR="$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd -P)"
# shellcheck source=../lib/common.sh
. "$E2E_DIR/lib/common.sh"
# shellcheck source=../lib/orchestration_fixtures.sh
. "$E2E_DIR/lib/orchestration_fixtures.sh"

SCENARIO="steering-queue-handoff"

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

wait_for_log() { # needle [timeout]
    local needle="$1" timeout="${2:-15}" deadline
    deadline=$((SECONDS + timeout))
    while ((SECONDS < deadline)); do
        if grep -F -- "$needle" "$evidence/mock-server.log" >/dev/null 2>&1; then
            return 0
        fi
        sleep 0.2
    done
    fail "user.$SCENARIO: mock log never showed ${needle@Q} (see $evidence/mock-server.log)"
}

mock_request_line() { # n -> last user-summary log line for request n
    # The user_mock_server logs one `... request#N user_len=... user_digest=...`
    # line per request (plus a later `kind=` line); the summary line is the
    # one that carries the queue-order digest markers.
    grep -F "request#$1 user_len=" "$evidence/mock-server.log" | tail -n 1 || true
}

digest_of() { # text -> 12-hex sha256 prefix (user_mock_server.py logs user_digest=...)
    printf '%s' "$1" | sha256sum | cut -c1-12
}

assert_mock_request_contains() { # n needle
    local line
    line="$(mock_request_line "$1")"
    case "$line" in
        *"$2"*) ;;
        *) fail "user.$SCENARIO: mock request #$1 must carry ${2@Q}; got: $line" ;;
    esac
}

assert_mock_request_lacks() { # n needle
    local line
    line="$(mock_request_line "$1")"
    case "$line" in
        *"$2"*) fail "user.$SCENARIO: mock request #$1 must NOT carry ${2@Q} (executed too early); got: $line" ;;
        *) ;;
    esac
}

assert_transcript_order() { # follow-up replies must follow their turns
    if ! awk '
        /steer-1-ing stream chunk-four-done/ { s1 = NR }
        /steer-3-ing stream chunk-four-done/ { s3 = NR }
        /steering-followup-reply/ { if (!r1) r1 = NR; r2 = NR }
        END { exit !(s1 && s3 && r1 && r2 && s1 < r1 && s3 < r2) }
    ' "$evidence/tui.txt"; then
        fail "user.$SCENARIO: follow-up replies must render after their turns complete (see $evidence/tui.txt)"
    fi
}

start_mock() { # root -> prints port
    local root="$1" port_file="$root/mock-port.txt" deadline port
    python3 "$E2E_DIR/lib/user_mock_server.py" --scenario steering --port-file "$port_file" \
        >"$evidence/mock-server.log" 2>&1 &
    register_pid $!
    deadline=$((SECONDS + 15))
    while [ ! -s "$port_file" ] && [ "$SECONDS" -lt "$deadline" ]; do sleep 0.2; done
    [ -s "$port_file" ] || fail "user.$SCENARIO: mock server did not write its port file"
    port="$(cat "$port_file")"
    printf '%s\n' "$port"
}

write_fake_xclip() { # bindir capture
    local bindir="$1" capture="$2"
    mkdir -p "$bindir"
    cat >"$bindir/xclip" <<EOF
#!/usr/bin/env bash
cat >"$capture"
exit 0
EOF
    chmod +x "$bindir/xclip"
}

main() {
    require_rpi
    require_cmd tmux
    require_cmd python3

    local root evidence session port bindir
    root="$(scenario_workspace "$SCENARIO")"
    evidence="$EVIDENCE_ROOT/$SCENARIO"
    session="$(unique_tmux_name "$SCENARIO")"
    bindir="$root/bin"
    register_tmux_session "$session"
    write_fake_xclip "$bindir" "$evidence/xclip-capture.txt"

    port="$(start_mock "$root")"
    cat >"$root/home/.pi/agent/models.json" <<EOF
{
  "providers": {
    "user-steering": {
      "baseUrl": "http://127.0.0.1:$port",
      "api": "openai-completions",
      "models": [
        { "id": "mock", "name": "Steering Mock", "contextWindow": 32768, "maxTokens": 2048 }
      ]
    }
  }
}
EOF

    tmux new-session -d -s "$session" -x 140 -y 42 -c "$root/workspace" \
        "env HOME='$root/home' USERPROFILE='$root/home' PATH='$bindir:/usr/bin:/bin' \
PI_CODING_AGENT_DIR='$root/home/.pi/agent' \
PI_OFFLINE=1 PI_SKIP_VERSION_CHECK=1 \
TERM=xterm-256color \
'$RPI_BIN' --offline --model user-steering/mock --api-key user-mock-key; printf '===TUI-DONE-steering-queue-handoff===\n'"

    tmux_wait_for "$session" 30 'user-steering/mock' >"$evidence/boot.txt" || true
    # Dismiss the startup resume selector when it is showing (harmless otherwise).
    tmux send-keys -t "$session":0 Escape
    sleep 0.3

    # --- Round 1: create a goal (starts turn 1 = slow stream), then steer ---
    send_cmd '/goal create --tokens 100 steer the batch'
    wait_for 'Objective: steer the batch' 25
    wait_for 'Status: active' 15
    wait_for 'steer-1-' 20
    # Turn 1 is streaming now: the follow-up must queue, not run as a prompt.
    send_cmd 'steering-followup-one'
    wait_for '⚙ 1' 15
    if ! tmux_wait_for "$session" 5 'Queued follow-up' >"$evidence/queued-status.txt"; then
        log "user.$SCENARIO: 'Queued follow-up' status already replaced; ⚙ count asserted"
    fi

    # Stream finishes -> follow-up drains automatically -> mock request 2.
    # Durable ordering proof (independent of any transient frame): request 1's
    # body was fixed when the turn started, so it must NOT carry the follow-up
    # digest; request 2 is the drained follow-up and MUST carry its digest as
    # its last user message. The transcript-order check below pins the same
    # contract.
    wait_for 'chunk-four-done' 25
    wait_for 'steering-followup-reply' 25
    wait_for_log 'request#2 ' 15
    assert_mock_request_lacks 1 "user_digest=$(digest_of 'steering-followup-one')"
    assert_mock_request_contains 2 "user_digest=$(digest_of 'steering-followup-one')"
    # The queue drained with the turn; /queue reports empty.
    send_cmd '/queue'
    wait_for 'Queue is empty' 15

    # --- Round 2: repeat the queue window, then cancel (idempotent) ---
    # Clear pane history so the ⚙ wait can only be satisfied by a fresh
    # header frame (round 1's queued window must not linger in scrollback).
    sleep 0.8
    tmux clear-history -t "$session":0
    send_cmd 'round two prompt'
    wait_for 'steer-3-' 20
    send_cmd 'steering-followup-two'
    wait_for '⚙ 1' 15
    if ! tmux_wait_for "$session" 5 'Queued follow-up' >"$evidence/queued-status-2.txt"; then
        log "user.$SCENARIO: 'Queued follow-up' status already replaced; ⚙ count asserted"
    fi
    # Drain: request 4 is the follow-up; request 3 (round 2's in-flight turn)
    # must never have carried it (its digest is absent).
    wait_for_log 'request#4 ' 25
    assert_mock_request_lacks 3 "user_digest=$(digest_of 'steering-followup-two')"
    assert_mock_request_contains 4 "user_digest=$(digest_of 'steering-followup-two')"
    send_cmd '/queue cancel'
    wait_for 'Queue is empty' 15
    send_cmd '/queue'
    wait_for 'Queue is empty' 15

    # --- /handoff: deterministic envelope in the transcript + clipboard ---
    send_cmd '/handoff'
    wait_for 'Handoff' 20
    wait_for 'Next steps' 20
    wait_for 'Copied handoff to clipboard' 20
    sleep 0.5
    assert_file_contains "$evidence/xclip-capture.txt" '# Handoff'

    tmux capture-pane -p -S -3000 -t "$session":0 >"$evidence/tui.txt" 2>/dev/null || true
    tmux send-keys -t "$session":0 -l '/quit'
    tmux send-keys -t "$session":0 Enter
    if ! tmux_wait_for "$session" 12 '===TUI-DONE-steering-queue-handoff===' >"$evidence/quit.txt"; then
        log "user.$SCENARIO: quit marker not seen; scenario assertions already passed, tearing down"
    fi
    tmux kill-session -t "$session" 2>/dev/null || true

    assert_file_contains "$evidence/tui.txt" \
        'steering-followup-reply' \
        'Next steps'
    # Durable drain ordering: each follow-up reply must render after the turn
    # it was queued behind completed (steer-1- / steer-3- streams), proving the
    # prompt was queued rather than executed immediately.
    assert_transcript_order
    printf 'user.%s passed\nevidence=%s\n' "$SCENARIO" "$evidence"
}

main "$@"
