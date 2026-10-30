#!/usr/bin/env bash
# TUI code-review submodule journey: `/code-review review-base` must expand the
# dirty `ext` gitlink into the child file (inner hunks, no Subproject block),
# keep the `sib-clean` gitlink row even though its clean bump commit changes
# tracked files (`notes.txt` must never appear), and fold/scroll/comment via keys.
# Tree fold/unfold uses Left/Right (`h` is globally Highlight); End scrolls.
set -euo pipefail
SCRIPT_DIR="$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
E2E_DIR="$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd -P)"
. "$E2E_DIR/lib/common.sh"
. "$E2E_DIR/lib/orchestration_fixtures.sh"

SCENARIO="code-review-submodule"
HIGHLIGHT_STREAM_MARKER="steer-1-"
MARKER="SUBMOD-P0-CHANGED"

wait_for() {
    local needle="$1" timeout="${2:-25}"
    if ! tmux_wait_for "$session" "$timeout" "$needle" >"$evidence/live.txt"; then
        fail "user.$SCENARIO: did not find ${needle@Q} (see $evidence/live.txt)"
    fi
}

wait_all() {
    local timeout="$1"
    shift
    if ! tmux_wait_for "$session" "$timeout" "$@" >"$evidence/live.txt"; then
        fail "user.$SCENARIO: did not find one of [$*] (see $evidence/live.txt)"
    fi
}

wait_gone() {
    local needle="$1" timeout="${2:-15}" deadline capture
    deadline=$((SECONDS + timeout))
    while ((SECONDS < deadline)); do
        capture="$(tmux capture-pane -p -S -2000 -t "$session":0 2>/dev/null)" \
            || fail "user.$SCENARIO: tmux pane capture failed while waiting for ${needle@Q} to disappear"
        case "$capture" in
            *"$needle"*) ;;
            *) return 0 ;;
        esac
        sleep 0.25
    done
    tmux capture-pane -p -S -2000 -t "$session":0 2>/dev/null \
        || fail "user.$SCENARIO: tmux pane capture failed while waiting for ${needle@Q} to disappear"
    return 1
}

start_mock() {
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

seed_submodule_fixture() {
    local root="$1" workspace="$1/workspace" status
    local gitenv=(env GIT_CONFIG_NOSYSTEM=1 GIT_CONFIG_GLOBAL="$root/absent-global-gitconfig" GIT_ALLOW_PROTOCOL=file)
    require_cmd git
    require_cmd python3
    mkdir -p "$root/subrepos/ext-src" "$root/subrepos/sib-src"

    "${gitenv[@]}" git -C "$root/subrepos/ext-src" init -q
    "${gitenv[@]}" git -C "$root/subrepos/ext-src" config user.email tui-e2e@example.com
    "${gitenv[@]}" git -C "$root/subrepos/ext-src" config user.name "TUI E2E"
    "${gitenv[@]}" git -C "$root/subrepos/ext-src" config commit.gpgsign false
    python3 - "$root/subrepos/ext-src/lib.txt" <<'PY'
import sys
with open(sys.argv[1], "w", encoding="utf-8") as fh:
    for number in range(1, 1005):
        fh.write(f"EXT-LIB-{number:04d}\n")
PY
    "${gitenv[@]}" git -C "$root/subrepos/ext-src" add lib.txt
    "${gitenv[@]}" git -C "$root/subrepos/ext-src" commit -q -m "ext submodule initial"

    "${gitenv[@]}" git -C "$root/subrepos/sib-src" init -q
    "${gitenv[@]}" git -C "$root/subrepos/sib-src" config user.email tui-e2e@example.com
    "${gitenv[@]}" git -C "$root/subrepos/sib-src" config user.name "TUI E2E"
    "${gitenv[@]}" git -C "$root/subrepos/sib-src" config commit.gpgsign false
    printf 'SIB-CLEAN-LIB-1\nSIB-CLEAN-LIB-2\nSIB-CLEAN-LIB-3\n' >"$root/subrepos/sib-src/lib.txt"
    "${gitenv[@]}" git -C "$root/subrepos/sib-src" add lib.txt
    "${gitenv[@]}" git -C "$root/subrepos/sib-src" commit -q -m "sib-clean submodule initial"

    "${gitenv[@]}" git -C "$workspace" init -q
    "${gitenv[@]}" git -C "$workspace" config user.email tui-e2e@example.com
    "${gitenv[@]}" git -C "$workspace" config user.name "TUI E2E"
    "${gitenv[@]}" git -C "$workspace" config commit.gpgsign false
    printf 'review root\n' >"$workspace/README.md"
    "${gitenv[@]}" git -C "$workspace" add README.md
    "${gitenv[@]}" git -C "$workspace" commit -q -m root
    "${gitenv[@]}" git -C "$workspace" -c protocol.file.allow=always submodule add -q "$root/subrepos/ext-src" ext
    "${gitenv[@]}" git -C "$workspace" commit -q -m "add ext submodule"
    "${gitenv[@]}" git -C "$workspace" -c protocol.file.allow=always submodule add -q "$root/subrepos/sib-src" sib-clean
    "${gitenv[@]}" git -C "$workspace" commit -q -m "add sib-clean submodule"
    "${gitenv[@]}" git -C "$workspace" branch review-base

    "${gitenv[@]}" git -C "$workspace/ext" -c user.email=tui-e2e@example.com -c user.name="TUI E2E" -c commit.gpgsign=false commit --allow-empty -q -m "ext submodule bump"
    printf 'SIB-NOTES-A\nSIB-NOTES-B\n' >"$workspace/sib-clean/notes.txt"
    "${gitenv[@]}" git -C "$workspace/sib-clean" -c user.email=tui-e2e@example.com -c user.name="TUI E2E" -c commit.gpgsign=false add notes.txt
    "${gitenv[@]}" git -C "$workspace/sib-clean" -c user.email=tui-e2e@example.com -c user.name="TUI E2E" -c commit.gpgsign=false commit -q -m "sib-clean submodule bump"

    python3 - "$workspace/ext/lib.txt" "$MARKER" <<'PY'
import sys
path, marker = sys.argv[1], sys.argv[2]
with open(path, encoding="utf-8") as fh:
    lines = fh.readlines()
for number in (40, 140, 240, 340, 440, 540):
    lines[number - 1] = f"EXT-LIB-{number:04d}-EDITED\n"
lines[999] = f"EXT-LIB-1000 {marker}\n"
with open(path, "w", encoding="utf-8") as fh:
    fh.writelines(lines)
PY

    [ -e "$workspace/ext/.git" ] || fail "fixture: ext submodule checkout missing"
    [ -f "$workspace/ext/lib.txt" ] || fail "fixture: ext/lib.txt missing"
    grep -F -q "$MARKER" "$workspace/ext/lib.txt" || fail "fixture: $MARKER missing in ext/lib.txt"
    [ -e "$workspace/sib-clean/.git" ] || fail "fixture: sib-clean checkout missing"
    status="$("${gitenv[@]}" git -C "$workspace" status --porcelain)"
    case "$status" in
        *" M ext"*) ;;
        *) fail "fixture: dirty ext gitlink not modified vs review-base: ${status:-<empty>}" ;;
    esac
    case "$status" in
        *" M sib-clean"*) ;;
        *) fail "fixture: bumped sib-clean gitlink not modified vs review-base: ${status:-<empty>}" ;;
    esac
    status="$("${gitenv[@]}" git -C "$workspace/ext" status --porcelain)"
    case "$status" in
        *" M lib.txt"*) ;;
        *) fail "fixture: ext inner lib.txt edit missing: ${status:-<empty>}" ;;
    esac
    status="$("${gitenv[@]}" git -C "$workspace/sib-clean" status --porcelain)"
    [ -z "$status" ] || fail "fixture: sib-clean checkout not clean: $status"
    [ -f "$workspace/sib-clean/notes.txt" ] || fail "fixture: sib-clean/notes.txt missing from checkout"
    changed="$("${gitenv[@]}" git -C "$workspace/sib-clean" diff-tree --no-commit-id --name-only -r HEAD)"
    [ "$changed" = "notes.txt" ] || fail "fixture: sib-clean bump commit must change tracked files, got: ${changed:-<empty>}"
}

main() {
    require_rpi
    require_cmd tmux
    require_cmd git
    require_cmd python3

    local root evidence session port
    root="$(scenario_workspace "$SCENARIO")"
    evidence="$EVIDENCE_ROOT/$SCENARIO"
    session="$(unique_tmux_name "$SCENARIO")"
    register_tmux_session "$session"
    seed_submodule_fixture "$root"
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
        "env HOME='$root/home' USERPROFILE='$root/home' \
PI_CODING_AGENT_DIR='$root/home/.pi/agent' \
RPI_OFFLINE=1 RPI_SKIP_VERSION_CHECK=1 \
TERM=xterm-256color \
'$RPI_BIN' --offline --model user-steering/mock --api-key user-mock-key; printf '===TUI-DONE-code-review-submodule===\n'" \
        \; set-option -t "$session" remain-on-exit on

    tmux_wait_for "$session" 25 'user-steering/mock' >"$evidence/boot.txt"
    tmux send-keys -t "$session":0 Escape

    tmux send-keys -t "$session":0 -l '/code-review review-base'
    wait_for '/code-review review-base' 10
    tmux send-keys -t "$session":0 Enter
    wait_all 40 'Code review' 'review-base → working tree' '2 files' 'focus:tree' \
        'sib-clean' 'lib.txt' '@@' 'e expand' 'EXT-LIB-0040-EDITED'
    tmux capture-pane -p -S -2000 -t "$session":0 >"$evidence/01-open.txt"
    assert_file_contains "$evidence/01-open.txt" \
        'Code review' 'review-base → working tree' '2 files' 'focus:tree' \
        'sib-clean' 'lib.txt' '@@' 'e expand' 'EXT-LIB-0040-EDITED'
    assert_file_lacks "$evidence/01-open.txt" 'Subproject commit' 'notes.txt'

    tmux clear-history -t "$session":0
    tmux send-keys -t "$session":0 h
    wait_for 'Highlight ■ Stop' 10
    tmux capture-pane -p -t "$session":0 >"$evidence/02-highlight-key.txt"
    assert_file_contains "$evidence/02-highlight-key.txt" 'Highlight ■ Stop' 'lib.txt' 'ext'
    assert_file_lacks "$evidence/02-highlight-key.txt" 'Select a file'

    tmux send-keys -t "$session":0 Left
    wait_for 'Select a file' 10
    tmux send-keys -t "$session":0 Left
    wait_gone 'lib.txt' 10
    tmux capture-pane -p -S -2000 -t "$session":0 >"$evidence/02-folded.txt"
    assert_file_contains "$evidence/02-folded.txt" 'ext' 'sib-clean'
    assert_file_lacks "$evidence/02-folded.txt" 'lib.txt' 'Subproject commit' 'notes.txt'

    tmux send-keys -t "$session":0 Right
    wait_for 'lib.txt' 10
    tmux send-keys -t "$session":0 Down
    tmux send-keys -t "$session":0 Right
    wait_all 15 'focus:diff' '@@' 'e expand' 'EXT-LIB-0040-EDITED'
    tmux capture-pane -p -S -2000 -t "$session":0 >"$evidence/03-diff-top.txt"
    assert_file_contains "$evidence/03-diff-top.txt" 'focus:diff' 'sib-clean' '@@' \
        'e expand' 'EXT-LIB-0040-EDITED'
    assert_file_lacks "$evidence/03-diff-top.txt" "$MARKER"

    tmux send-keys -t "$session":0 End
    wait_for "$MARKER" 10
    tmux capture-pane -p -S -2000 -t "$session":0 >"$evidence/04-scrolled.txt"
    assert_file_contains "$evidence/04-scrolled.txt" "$MARKER" '@@' 'EXT-LIB-1000' \
        'focus:diff' 'sib-clean'

    tmux send-keys -t "$session":0 c
    wait_all 10 'Enter submit' 'Esc cancel'
    tmux capture-pane -p -S -2000 -t "$session":0 >"$evidence/05-comment.txt"
    assert_file_contains "$evidence/05-comment.txt" 'Enter submit' 'Esc cancel' 'focus:diff' '@@ -997,7'
    assert_file_lacks "$evidence/05-comment.txt" 'Comment 1'

    tmux send-keys -t "$session":0 Escape
    wait_gone 'Enter submit' 10
    tmux capture-pane -p -S -2000 -t "$session":0 >"$evidence/06-comment-cancel.txt"
    assert_file_contains "$evidence/06-comment-cancel.txt" 'focus:diff' "$MARKER"
    assert_file_lacks "$evidence/06-comment-cancel.txt" \
        'Enter submit' 'Comment 1' 'comment submitted' "$HIGHLIGHT_STREAM_MARKER"
    assert_file_contains "$evidence/mock-server.log" 'request#1 '
    assert_file_lacks "$evidence/mock-server.log" 'request#2 '

    tmux send-keys -t "$session":0 q
    wait_gone 'review-base → working tree' 10
    tmux capture-pane -p -S -2000 -t "$session":0 >"$evidence/07-closed.txt"
    assert_file_lacks "$evidence/07-closed.txt" 'review-base → working tree' 'focus:tree' 'focus:diff'

    tmux send-keys -t "$session":0 -l '/quit'
    wait_for '/quit' 10
    tmux send-keys -t "$session":0 Enter
    if ! tmux_wait_for "$session" 15 '===TUI-DONE-code-review-submodule===' >"$evidence/08-quit.txt"; then
        fail "user.$SCENARIO: TUI did not exit after /quit (see $evidence/08-quit.txt)"
    fi
    printf 'user.%s passed\nevidence=%s\n' "$SCENARIO" "$evidence"
}

main "$@"
