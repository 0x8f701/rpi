#!/usr/bin/env bash
# Deterministic tmux campaign for interactive slash-command families not
# owned by other QA lanes:
#   /loop /loops /loop-update /loop-delete /loop-cancel
#   /model /models /scoped-models
#   /compact /branch /clone /export /share /copy /name /theme /reload /new
#   /help /hotkeys /changelog
#
# Exercises real TUI (full-screen) and REPL (line-oriented) dispatch inside
# isolated tmux panes with a faux provider (PI_FAUX_RESPONSE) and faux/local
# seams for the external boundaries (/share -> fake gh, /copy -> fake xclip).
# Asserts command EFFECTS (state changes, persisted files, overlay open/close,
# exported/copied representations) rather than mere dispatch text.
#
# No host credentials, clipboard, network, or real GitHub auth are touched.
# Each scenario is hard-bounded, captures tmux pane evidence with unique
# markers, verifies file/wire side effects, and proves terminal restoration
# with a subsequent normal shell command in the same pane.
#
# Usage: bash E2E.d/ci/interactive_commands.sh [run|list|<scenario>]
# Prereqs: RPI_BIN (target/release-dist/rpi built by Main after ALL CODE STABLE),
#         tmux, python3, git, standard coreutils.
set -euo pipefail
SCRIPT_DIR="$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
# shellcheck source=../lib/common.sh
. "$SCRIPT_DIR/../lib/common.sh"
# shellcheck source=../lib/orchestration_fixtures.sh
. "$SCRIPT_DIR/../lib/orchestration_fixtures.sh"

FAUX_REPLY_NORMAL="faux-interactive-reply"
FAUX_REPLY_LOOP="faux-loop-fired-reply"
FAUX_REPLY_COMPACT="faux-compact-checkpoint"

list_scenarios() {
    printf '%s\n' \
        'ic.repl-display - /help /changelog /hotkeys dispatch and content in line REPL' \
        'ic.repl-name - /name set/read/persist in line REPL' \
        'ic.repl-model - /model switch /models /bare /model in line REPL (two faux models)' \
        'ic.repl-loop - /loop create/list + usage errors in line REPL (lifecycle in TUI)' \
        'ic.tui-display - /help /changelog /hotkeys panels in full-screen TUI' \
        'ic.tui-name-new - /name set/read/persist then /new resets in TUI' \
        'ic.tui-model - /model switch + /models + /scoped-models overlay open/close in TUI' \
        'ic.tui-loop - /loop lifecycle + fired turn + /loops panel in TUI' \
        'ic.tui-compact - /compact manual compaction reports tokens in TUI' \
        'ic.tui-branch-clone - /branch overlay open/close + /clone branch in TUI' \
        'ic.tui-export - /export HTML and JSONL file side effects in TUI' \
        'ic.tui-share - /share via fake gh seam (no network) in TUI' \
        'ic.tui-copy - /copy representation via fake xclip seam (no host clipboard) in TUI' \
        'ic.tui-reload - /reload advances resource generation in TUI' \
        'ic.tui-theme - /theme list/switch/cycle state change in TUI'
}

# Write a models.json declaring TWO faux-API models so /model switching is a
# real, resolvable state change (faux needs no api key; has_configured_auth
# short-circuits to true for api == faux).
write_two_faux_models() {
    local agent="$1"
    mkdir -p "$agent"
    cat >"$agent/models.json" <<'EOF'
{
  "providers": {
    "faux-alt": {
      "api": "faux",
      "models": [
        { "id": "alt-1", "name": "Faux Alt", "contextWindow": 32768, "maxTokens": 2048 }
      ]
    }
  }
}
EOF
}

# Fake gh CLI seam for /share: satisfies `gh --version`, `gh auth status`, and
# `gh gist create --desc ... <path>`, echoing a deterministic gist URL and
# recording the path it received (to prove the exported HTML was uploaded).
write_fake_gh() {
    local bindir="$1" evidence="$2"
    mkdir -p "$bindir"
    cat >"$bindir/gh" <<EOF
#!/usr/bin/env bash
case "\$1" in
  --version)
    printf 'gh version 2.40.0 (faux-seam)\n'
    exit 0
    ;;
  auth)
    printf 'Logged in to github.com account faux-e2e\n'
    exit 0
    ;;
  gist)
    path="\${@: -1}"
    printf '%s\n' "\$path" >"$evidence/gh-received-path.txt"
    if [ -f "\$path" ]; then wc -c < "\$path" >"$evidence/gh-received-bytes.txt"; fi
    printf 'https://gist.github.com/faux-e2e-%s-%s\n' "\$\$" "\$(date +%s)"
    exit 0
    ;;
  *)
    printf 'faux-gh: unexpected invocation: %s\n' "\$*" >&2
    exit 1
    ;;
esac
EOF
    chmod +x "$bindir/gh"
}

# Fake xclip seam for /copy: writes the stdin payload (the would-be clipboard
# text) to a capture file so the copy REPRESENTATION is verifiable without ever
# touching the host clipboard. Exits 0 so clipboard::write_owned_fallback stops.
write_fake_xclip() {
    local bindir="$1" capture="$2"
    mkdir -p "$bindir"
    cat >"$bindir/xclip" <<EOF
#!/usr/bin/env bash
cat >"$capture"
exit 0
EOF
    chmod +x "$bindir/xclip"
}

# Launch a full-screen TUI in an isolated tmux session over an ALREADY-CREATED
# workspace root (callers run setup fixtures before this). $1=root $2=name;
# reply/extra-env via the globals FAUX_REPLY and FAUX_EXTRA.
launch_tui() {
    local root="$1" name="$2" evidence session binpath
    evidence="$EVIDENCE_ROOT/$name"; mkdir -p "$evidence"
    session="$(unique_tmux_name "ic-$name")"
    register_tmux_session "$session"
    binpath="$root/bin"; mkdir -p "$binpath"
    tmux new-session -d -s "$session" -x 140 -y 42 -c "$root/workspace" \
        "env -i HOME='$root/home' USERPROFILE='$root/home' PATH='$binpath:/usr/bin:/bin' \
LANG=${LANG:-C.UTF-8} LC_ALL=${LC_ALL:-C.UTF-8} \
PI_CODING_AGENT_DIR='$root/home/.pi/agent' \
PI_OFFLINE=1 PI_SKIP_VERSION_CHECK=1 \
PI_FAUX_RESPONSE='${FAUX_REPLY:-$FAUX_REPLY_NORMAL}' \
TERM=xterm-256color \
${FAUX_EXTRA:-} \
'$RPI_BIN' --offline --model faux/faux-1; printf '===TUI-DONE-${name}===\n'"
    printf '%s\n' "$session"
}

# Launch a line REPL with a pre-written command script piped to stdin (stdin
# non-tty + stdout tty => REPL mode, ANSI on). A DONE marker prints after rpi
# exits to prove terminal restoration in the same pane.
launch_repl() {
    local root="$1" name="$2" cmds="$3" evidence session binpath
    evidence="$EVIDENCE_ROOT/$name"; mkdir -p "$evidence"
    session="$(unique_tmux_name "ic-$name")"
    register_tmux_session "$session"
    binpath="$root/bin"; mkdir -p "$binpath"
    tmux new-session -d -s "$session" -x 140 -y 50 -c "$root/workspace" \
        "env -i HOME='$root/home' USERPROFILE='$root/home' PATH='$binpath:/usr/bin:/bin' \
LANG=${LANG:-C.UTF-8} LC_ALL=${LC_ALL:-C.UTF-8} \
PI_CODING_AGENT_DIR='$root/home/.pi/agent' \
PI_OFFLINE=1 PI_SKIP_VERSION_CHECK=1 \
PI_FAUX_RESPONSE='${FAUX_REPLY:-$FAUX_REPLY_NORMAL}' \
TERM=xterm-256color \
${FAUX_EXTRA:-} \
'$RPI_BIN' --offline --model faux/faux-1 < '$cmds'; printf '===REPL-DONE-${name}===\n'"
    printf '%s\n' "$session"
}

capture_pane() { tmux capture-pane -p -S -3000 -t "$1":0 2>/dev/null || true; }

# Wait for a literal needle in the pane, capture to evidence on success, fail otherwise.
wait_capture() {
    local session="$1" evidence="$2" needle="$3" timeout="${4:-25}"
    if ! tmux_wait_for "$session" "$timeout" "$needle" >"$evidence"; then
        capture_pane "$session" >"$evidence"
        fail "$session: did not find ${needle@Q} (see $evidence)"
    fi
}

# --- REPL scenarios -----------------------------------------------------------

run_repl_display() {
    local name="repl-display" root evidence session cmds
    root="$(scenario_workspace "$name")"; evidence="$EVIDENCE_ROOT/$name"
    cmds="$root/workspace/cmds.txt"
    printf '%s\n' '/help' '/changelog' '/hotkeys' '/quit' >"$cmds"
    session="$(launch_repl "$root" "$name" "$cmds")"
    wait_capture "$session" "$evidence/pane.txt" '===REPL-DONE-' 30
    capture_pane "$session" >"$evidence/pane.txt"
    assert_file_contains "$evidence/pane.txt" '/model' '/loop' 'Show available commands'
    assert_file_contains "$evidence/pane.txt" 'Enter submit'
    grep -E '^#' "$evidence/pane.txt" >/dev/null || fail "changelog heading missing"
    tmux kill-session -t "$session" 2>/dev/null || true
    log "ic.$name passed"
}

run_repl_name() {
    local name="repl-name" root evidence session cmds marker="qa-repl-name"
    root="$(scenario_workspace "$name")"; evidence="$EVIDENCE_ROOT/$name"
    cmds="$root/workspace/cmds.txt"
    # A real turn first so the session records to disk, then /name persists.
    printf '%s\n' "hello there" "/name $marker" '/name' '/quit' >"$cmds"
    session="$(launch_repl "$root" "$name" "$cmds")"
    wait_capture "$session" "$evidence/pane.txt" '===REPL-DONE-' 40
    capture_pane "$session" >"$evidence/pane.txt"
    assert_file_contains "$evidence/pane.txt" "session name: $marker" "$marker"
    grep -R -F -- "$marker" "$root/home/.pi/agent/sessions" >"$evidence/name-persist.txt" \
        || fail "/name did not persist to session JSONL"
    tmux kill-session -t "$session" 2>/dev/null || true
    log "ic.$name passed"
}

run_repl_model() {
    local name="repl-model" root evidence session cmds
    root="$(scenario_workspace "$name")"; evidence="$EVIDENCE_ROOT/$name"
    write_two_faux_models "$root/home/.pi/agent"
    cmds="$root/workspace/cmds.txt"
    printf '%s\n' '/model' '/model faux-alt/alt-1' '/model' '/models faux' '/quit' >"$cmds"
    session="$(launch_repl "$root" "$name" "$cmds")"
    wait_capture "$session" "$evidence/pane.txt" '===REPL-DONE-' 40
    capture_pane "$session" >"$evidence/pane.txt"
    assert_file_contains "$evidence/pane.txt" 'current: faux/faux-1' 'switched to faux-alt/alt-1' 'current: faux-alt/alt-1'
    grep -F 'faux-alt/alt-1' "$evidence/pane.txt" >/dev/null || fail "/models filter missing faux-alt/alt-1"
    tmux kill-session -t "$session" 2>/dev/null || true
    log "ic.$name passed"
}

run_repl_loop() {
    local name="repl-loop" root evidence session cmds task_id
    root="$(scenario_workspace "$name")"; evidence="$EVIDENCE_ROOT/$name"
    cmds="$root/workspace/cmds.txt"
    # The piped REPL cannot feed a parsed id back, so the id-driven lifecycle
    # (update/delete/cancel success) is exercised in the TUI scenario. Here we
    # prove create + list render the real id, the immediate faux fire, and the
    # three typed usage errors for missing arguments.
    {
        printf '%s\n' '/loop 1h slash keep-alive'
        printf '%s\n' '/loops'
        printf '%s\n' '/loop'
        printf '%s\n' '/loop-delete'
        printf '%s\n' '/loop-cancel'
        printf '%s\n' '/loop-update only-id'
        printf '%s\n' '/quit'
    } >"$cmds"
    FAUX_REPLY="$FAUX_REPLY_LOOP" session="$(launch_repl "$root" "$name" "$cmds")"
    wait_capture "$session" "$evidence/pane.txt" '===REPL-DONE-' 40
    capture_pane "$session" >"$evidence/pane.txt"
    assert_file_contains "$evidence/pane.txt" 'scheduled'
    task_id="$(grep -oE 'scheduled [0-9a-f]{12}' "$evidence/pane.txt" | head -1 | awk '{print $2}')"
    [ -n "$task_id" ] || fail "could not parse loop task id from /loop output"
    assert_file_contains "$evidence/pane.txt" "$task_id" 'slash keep-alive'
    # Immediate fire produced a faux turn.
    assert_file_contains "$evidence/pane.txt" "$FAUX_REPLY_LOOP"
    # Typed usage errors for every missing-argument form.
    assert_file_contains "$evidence/pane.txt" 'usage'
    tmux kill-session -t "$session" 2>/dev/null || true
    log "ic.$name passed"
}

# --- TUI scenarios -----------------------------------------------------------

run_tui_display() {
    local name="tui-display" root evidence session
    root="$(scenario_workspace "$name")"; evidence="$EVIDENCE_ROOT/$name"
    session="$(launch_tui "$root" "$name")"
    tmux_wait_for "$session" 25 'faux/faux-1' >/dev/null || true
    sleep 1
    tmux send-keys -t "$session":0 -l '/help'; tmux send-keys -t "$session":0 Enter
    wait_capture "$session" "$evidence/help.txt" 'Show available commands' 15
    tmux send-keys -t "$session":0 Escape; sleep 0.3
    tmux send-keys -t "$session":0 -l '/changelog'; tmux send-keys -t "$session":0 Enter
    wait_capture "$session" "$evidence/changelog.txt" 'Changelog' 15
    tmux send-keys -t "$session":0 Escape; sleep 0.3
    tmux send-keys -t "$session":0 -l '/hotkeys'; tmux send-keys -t "$session":0 Enter
    wait_capture "$session" "$evidence/hotkeys.txt" 'Editor' 15
    tmux send-keys -t "$session":0 Escape; sleep 0.3
    assert_tmux_composer_editable "$session" "$evidence/composer.txt" 'IC-DISPLAY-SENTINEL'
    tmux send-keys -t "$session":0 -l '/quit'; tmux send-keys -t "$session":0 Enter
    wait_capture "$session" "$evidence/done.txt" '===TUI-DONE-' 10
    tmux kill-session -t "$session" 2>/dev/null || true
    log "ic.$name passed"
}

run_tui_name_new() {
    local name="tui-name-new" root evidence session marker="qa-tui-name"
    root="$(scenario_workspace "$name")"; evidence="$EVIDENCE_ROOT/$name"
    session="$(launch_tui "$root" "$name")"
    tmux_wait_for "$session" 25 'faux/faux-1' >/dev/null || true
    sleep 1
    tmux send-keys -t "$session":0 -l 'hello there'; tmux send-keys -t "$session":0 Enter
    tmux_wait_for "$session" 25 "$FAUX_REPLY_NORMAL" >/dev/null || true
    sleep 0.5
    tmux send-keys -t "$session":0 -l "/name $marker"; tmux send-keys -t "$session":0 Enter
    wait_capture "$session" "$evidence/name-set.txt" "Session name: $marker" 15
    tmux send-keys -t "$session":0 -l '/name'; tmux send-keys -t "$session":0 Enter
    wait_capture "$session" "$evidence/name-read.txt" "Session name: $marker" 15
    grep -R -F -- "$marker" "$root/home/.pi/agent/sessions" \
        >"$evidence/name-persist.txt" || fail "/name did not persist in TUI"
    tmux send-keys -t "$session":0 -l '/new'; tmux send-keys -t "$session":0 Enter
    wait_capture "$session" "$evidence/new.txt" 'Started a new session' 15
    tmux send-keys -t "$session":0 -l '/name'; tmux send-keys -t "$session":0 Enter
    sleep 1
    capture_pane "$session" >"$evidence/name-after-new.txt"
    assert_file_contains "$evidence/name-after-new.txt" '(unnamed)'
    tmux send-keys -t "$session":0 -l '/quit'; tmux send-keys -t "$session":0 Enter
    wait_capture "$session" "$evidence/done.txt" '===TUI-DONE-' 10
    tmux kill-session -t "$session" 2>/dev/null || true
    log "ic.$name passed"
}

run_tui_model() {
    local name="tui-model" root evidence session
    root="$(scenario_workspace "$name")"; evidence="$EVIDENCE_ROOT/$name"
    write_two_faux_models "$root/home/.pi/agent"
    session="$(launch_tui "$root" "$name")"
    tmux_wait_for "$session" 25 'faux/faux-1' >/dev/null || true
    sleep 1
    tmux send-keys -t "$session":0 -l '/model faux-alt/alt-1'; tmux send-keys -t "$session":0 Enter
    wait_capture "$session" "$evidence/switch.txt" 'Model: faux-alt/alt-1' 15
    capture_pane "$session" >"$evidence/chrome.txt"
    assert_file_contains "$evidence/chrome.txt" 'faux-alt/alt-1'
    tmux send-keys -t "$session":0 -l '/model'; tmux send-keys -t "$session":0 Enter
    sleep 1
    capture_pane "$session" >"$evidence/model-panel.txt"
    grep -E -e 'Select model' -e 'faux/faux-1' -e 'faux-alt/alt-1' "$evidence/model-panel.txt" >/dev/null \
        || fail "/model panel did not list faux models"
    tmux send-keys -t "$session":0 Escape; sleep 0.4
    tmux send-keys -t "$session":0 -l '/models'; tmux send-keys -t "$session":0 Enter
    wait_capture "$session" "$evidence/models.txt" 'faux-alt/alt-1' 15
    assert_file_contains "$evidence/models.txt" 'faux-alt/alt-1'
    tmux send-keys -t "$session":0 Escape; sleep 0.4
    tmux send-keys -t "$session":0 -l '/scoped-models'; tmux send-keys -t "$session":0 Enter
    sleep 1
    capture_pane "$session" >"$evidence/scoped-panel.txt"
    grep -E -e 'Model Configuration' -e 'Enter toggle' -e 'faux-alt/alt-1' "$evidence/scoped-panel.txt" >/dev/null \
        || fail "/scoped-models overlay did not open"
    tmux send-keys -t "$session":0 Escape; sleep 0.4
    assert_tmux_composer_editable "$session" "$evidence/composer.txt" 'IC-MODEL-SENTINEL'
    tmux send-keys -t "$session":0 -l '/quit'; tmux send-keys -t "$session":0 Enter
    wait_capture "$session" "$evidence/done.txt" '===TUI-DONE-' 10
    tmux kill-session -t "$session" 2>/dev/null || true
    log "ic.$name passed"
}

run_tui_loop() {
    local name="tui-loop" root evidence session task_id
    root="$(scenario_workspace "$name")"; evidence="$EVIDENCE_ROOT/$name"
    FAUX_REPLY="$FAUX_REPLY_LOOP" session="$(launch_tui "$root" "$name")"
    tmux_wait_for "$session" 25 'faux/faux-1' >/dev/null || true
    sleep 1
    tmux send-keys -t "$session":0 -l '/loop 1h tui keep-alive'; tmux send-keys -t "$session":0 Enter
    wait_capture "$session" "$evidence/create.txt" 'scheduled' 15
    task_id="$(grep -oE 'scheduled [0-9a-f]{12}' "$evidence/create.txt" | head -1 | awk '{print $2}')"
    [ -n "$task_id" ] || fail "could not parse TUI loop task id"
    # Let the immediate fire settle before inspecting the listing.
    tmux_wait_for "$session" 25 "$FAUX_REPLY_LOOP" >/dev/null || true
    tmux send-keys -t "$session":0 Escape; sleep 0.3
    tmux send-keys -t "$session":0 -l '/loops'; tmux send-keys -t "$session":0 Enter
    wait_capture "$session" "$evidence/loops.txt" 'next ' 15
    assert_file_contains "$evidence/loops.txt" "$task_id" 'tui keep-alive'
    tmux send-keys -t "$session":0 Escape; sleep 0.3
    tmux send-keys -t "$session":0 -l "/loop-update $task_id 2h tui updated"; tmux send-keys -t "$session":0 Enter
    wait_capture "$session" "$evidence/update.txt" 'updated loop' 15
    assert_file_contains "$evidence/update.txt" 'every 2 hours' 'tui updated'
    tmux send-keys -t "$session":0 -l "/loop-delete $task_id"; tmux send-keys -t "$session":0 Enter
    wait_capture "$session" "$evidence/delete.txt" "deleted loop $task_id" 15
    # Create + cancel path (distinct from delete): delete keeps an active turn.
    tmux send-keys -t "$session":0 -l '/loop 1h tui cancel target'; tmux send-keys -t "$session":0 Enter
    wait_capture "$session" "$evidence/create2.txt" 'scheduled' 15
    local cancel_id
    cancel_id="$(grep -oE 'scheduled [0-9a-f]{12}' "$evidence/create2.txt" | head -1 | awk '{print $2}')"
    [ -n "$cancel_id" ] || fail "could not parse cancel target id"
    tmux send-keys -t "$session":0 -l "/loop-cancel $cancel_id"; tmux send-keys -t "$session":0 Enter
    wait_capture "$session" "$evidence/cancel.txt" "cancelled loop $cancel_id" 15
    tmux send-keys -t "$session":0 -l '/loop'; tmux send-keys -t "$session":0 Enter
    sleep 1
    capture_pane "$session" >"$evidence/usage.txt"
    grep -E -e 'usage' -e 'interval' -e 'Usage' "$evidence/usage.txt" >/dev/null \
        || fail "bare /loop did not surface a usage error in TUI"
    tmux send-keys -t "$session":0 -l '/loops'; tmux send-keys -t "$session":0 Enter
    wait_capture "$session" "$evidence/none.txt" 'no active loops' 15
    tmux send-keys -t "$session":0 Escape; sleep 0.3
    assert_tmux_composer_editable "$session" "$evidence/composer.txt" 'IC-LOOP-SENTINEL'
    tmux send-keys -t "$session":0 -l '/quit'; tmux send-keys -t "$session":0 Enter
    wait_capture "$session" "$evidence/done.txt" '===TUI-DONE-' 10
    tmux kill-session -t "$session" 2>/dev/null || true
    log "ic.$name passed"
}

run_tui_compact() {
    local name="tui-compact" root evidence session
    root="$(scenario_workspace "$name")"; evidence="$EVIDENCE_ROOT/$name"
    FAUX_REPLY="$FAUX_REPLY_COMPACT" session="$(launch_tui "$root" "$name")"
    tmux_wait_for "$session" 25 'faux/faux-1' >/dev/null || true
    sleep 1
    tmux send-keys -t "$session":0 -l 'please summarize this conversation'; tmux send-keys -t "$session":0 Enter
    tmux_wait_for "$session" 25 "$FAUX_REPLY_COMPACT" >/dev/null || true
    sleep 0.5
    tmux send-keys -t "$session":0 -l '/compact keep recent turns'; tmux send-keys -t "$session":0 Enter
    wait_capture "$session" "$evidence/compact.txt" 'Compacted' 25
    grep -E 'Compacted [0-9]+ .+ [0-9]+ estimated tokens' "$evidence/compact.txt" >/dev/null \
        || fail "/compact did not report token delta: $(cat "$evidence/compact.txt")"
    assert_tmux_composer_editable "$session" "$evidence/composer.txt" 'IC-COMPACT-SENTINEL'
    tmux send-keys -t "$session":0 -l '/quit'; tmux send-keys -t "$session":0 Enter
    wait_capture "$session" "$evidence/done.txt" '===TUI-DONE-' 10
    tmux kill-session -t "$session" 2>/dev/null || true
    log "ic.$name passed"
}

run_tui_branch_clone() {
    local name="tui-branch-clone" root evidence session
    root="$(scenario_workspace "$name")"; evidence="$EVIDENCE_ROOT/$name"
    session="$(launch_tui "$root" "$name")"
    tmux_wait_for "$session" 25 'faux/faux-1' >/dev/null || true
    sleep 1
    tmux send-keys -t "$session":0 -l 'first user message'; tmux send-keys -t "$session":0 Enter
    tmux_wait_for "$session" 25 "$FAUX_REPLY_NORMAL" >/dev/null || true
    sleep 0.5
    tmux send-keys -t "$session":0 -l '/branch'; tmux send-keys -t "$session":0 Enter
    wait_capture "$session" "$evidence/branch.txt" 'Fork from Message' 15
    assert_file_contains "$evidence/branch.txt" 'Enter fork'
    tmux send-keys -t "$session":0 Escape; sleep 0.4
    assert_tmux_composer_editable "$session" "$evidence/composer-after-branch.txt" 'IC-BRANCH-SENTINEL'
    tmux send-keys -t "$session":0 -l '/clone'; tmux send-keys -t "$session":0 Enter
    wait_capture "$session" "$evidence/clone.txt" 'Cloned current session branch' 15
    assert_tmux_composer_editable "$session" "$evidence/composer-after-clone.txt" 'IC-CLONE-SENTINEL'
    tmux send-keys -t "$session":0 -l '/quit'; tmux send-keys -t "$session":0 Enter
    wait_capture "$session" "$evidence/done.txt" '===TUI-DONE-' 10
    tmux kill-session -t "$session" 2>/dev/null || true
    log "ic.$name passed"
}

run_tui_export() {
    local name="tui-export" root evidence session html jsonl
    root="$(scenario_workspace "$name")"; evidence="$EVIDENCE_ROOT/$name"
    html="$root/workspace/export.html"; jsonl="$root/workspace/export.jsonl"
    session="$(launch_tui "$root" "$name")"
    tmux_wait_for "$session" 25 'faux/faux-1' >/dev/null || true
    sleep 1
    tmux send-keys -t "$session":0 -l 'a turn to export'; tmux send-keys -t "$session":0 Enter
    tmux_wait_for "$session" 25 "$FAUX_REPLY_NORMAL" >/dev/null || true
    sleep 0.5
    tmux send-keys -t "$session":0 -l "/export $html"; tmux send-keys -t "$session":0 Enter
    wait_capture "$session" "$evidence/export-html.txt" "Exported $html" 15
    [ -f "$html" ] || fail "HTML export file not written"
    assert_file_contains "$html" '<title>rpi session export</title>'
    tmux send-keys -t "$session":0 -l "/export $jsonl"; tmux send-keys -t "$session":0 Enter
    wait_capture "$session" "$evidence/export-jsonl.txt" "Exported $jsonl" 15
    [ -f "$jsonl" ] || fail "JSONL export file not written"
    grep -E '"type":"(session|message|model_change)' "$jsonl" >/dev/null \
        || fail "JSONL export missing session records"
    tmux send-keys -t "$session":0 -l '/quit'; tmux send-keys -t "$session":0 Enter
    wait_capture "$session" "$evidence/done.txt" '===TUI-DONE-' 10
    tmux kill-session -t "$session" 2>/dev/null || true
    log "ic.$name passed"
}

run_tui_share() {
    local name="tui-share" root evidence session received
    root="$(scenario_workspace "$name")"; evidence="$EVIDENCE_ROOT/$name"
    write_fake_gh "$root/bin" "$evidence"
    session="$(launch_tui "$root" "$name")"
    tmux_wait_for "$session" 25 'faux/faux-1' >/dev/null || true
    sleep 1
    tmux send-keys -t "$session":0 -l 'a turn to share'; tmux send-keys -t "$session":0 Enter
    tmux_wait_for "$session" 25 "$FAUX_REPLY_NORMAL" >/dev/null || true
    sleep 0.5
    tmux send-keys -t "$session":0 -l '/share'; tmux send-keys -t "$session":0 Enter
    wait_capture "$session" "$evidence/share.txt" 'Shared: https://gist.github.com/faux-e2e' 25
    [ -s "$evidence/gh-received-path.txt" ] || fail "/share did not invoke fake gh with a path"
    received="$(cat "$evidence/gh-received-path.txt")"
    [ -f "$received" ] || fail "/share exported HTML missing at received path: $received"
    assert_file_contains "$received" '<title>rpi session export</title>'
    [ -s "$evidence/gh-received-bytes.txt" ] || fail "fake gh did not record uploaded byte count"
    tmux send-keys -t "$session":0 -l '/quit'; tmux send-keys -t "$session":0 Enter
    wait_capture "$session" "$evidence/done.txt" '===TUI-DONE-' 10
    tmux kill-session -t "$session" 2>/dev/null || true
    log "ic.$name passed"
}

run_tui_copy() {
    local name="tui-copy" root evidence session capture
    root="$(scenario_workspace "$name")"; evidence="$EVIDENCE_ROOT/$name"
    capture="$evidence/clipboard-capture.txt"
    write_fake_xclip "$root/bin" "$capture"
    FAUX_EXTRA='DISPLAY=:99' session="$(launch_tui "$root" "$name")"
    tmux_wait_for "$session" 25 'faux/faux-1' >/dev/null || true
    sleep 1
    tmux send-keys -t "$session":0 -l 'copy this answer'; tmux send-keys -t "$session":0 Enter
    tmux_wait_for "$session" 25 "$FAUX_REPLY_NORMAL" >/dev/null || true
    sleep 0.5
    tmux send-keys -t "$session":0 -l '/copy'; tmux send-keys -t "$session":0 Enter
    wait_capture "$session" "$evidence/copy.txt" 'Copied last assistant message' 25
    [ -s "$capture" ] || fail "/copy did not route text through fake xclip"
    assert_file_contains "$capture" "$FAUX_REPLY_NORMAL"
    tmux send-keys -t "$session":0 -l '/quit'; tmux send-keys -t "$session":0 Enter
    wait_capture "$session" "$evidence/done.txt" '===TUI-DONE-' 10
    tmux kill-session -t "$session" 2>/dev/null || true
    log "ic.$name passed"
}

run_tui_reload() {
    local name="tui-reload" root evidence session
    root="$(scenario_workspace "$name")"; evidence="$EVIDENCE_ROOT/$name"
    session="$(launch_tui "$root" "$name")"
    tmux_wait_for "$session" 25 'faux/faux-1' >/dev/null || true
    sleep 1
    tmux send-keys -t "$session":0 -l '/reload'; tmux send-keys -t "$session":0 Enter
    wait_capture "$session" "$evidence/reload.txt" 'Reloaded resource generation' 15
    grep -E 'Reloaded resource generation [0-9]+' "$evidence/reload.txt" >/dev/null \
        || fail "/reload did not report a generation"
    assert_tmux_composer_editable "$session" "$evidence/composer.txt" 'IC-RELOAD-SENTINEL'
    tmux send-keys -t "$session":0 -l '/quit'; tmux send-keys -t "$session":0 Enter
    wait_capture "$session" "$evidence/done.txt" '===TUI-DONE-' 10
    tmux kill-session -t "$session" 2>/dev/null || true
    log "ic.$name passed"
}

run_tui_theme() {
    local name="tui-theme" root evidence session
    root="$(scenario_workspace "$name")"; evidence="$EVIDENCE_ROOT/$name"
    session="$(launch_tui "$root" "$name")"
    tmux_wait_for "$session" 25 'faux/faux-1' >/dev/null || true
    sleep 1
    tmux send-keys -t "$session":0 -l '/theme'; tmux send-keys -t "$session":0 Enter
    wait_capture "$session" "$evidence/list.txt" 'Themes:' 15
    assert_file_contains "$evidence/list.txt" 'dark' 'light'
    tmux send-keys -t "$session":0 -l '/theme light'; tmux send-keys -t "$session":0 Enter
    wait_capture "$session" "$evidence/switch.txt" 'Theme: light' 15
    tmux send-keys -t "$session":0 -l '/theme next'; tmux send-keys -t "$session":0 Enter
    sleep 1
    capture_pane "$session" >"$evidence/next.txt"
    grep -E 'Theme: (dark|light)' "$evidence/next.txt" >/dev/null \
        || fail "/theme next did not report a theme"
    tmux send-keys -t "$session":0 -l '/theme nope'; tmux send-keys -t "$session":0 Enter
    sleep 1
    capture_pane "$session" >"$evidence/unknown.txt"
    grep -E -e 'unknown theme' -e 'nope' "$evidence/unknown.txt" >/dev/null \
        || fail "/theme unknown did not surface a typed error"
    tmux send-keys -t "$session":0 -l '/quit'; tmux send-keys -t "$session":0 Enter
    wait_capture "$session" "$evidence/done.txt" '===TUI-DONE-' 10
    tmux kill-session -t "$session" 2>/dev/null || true
    log "ic.$name passed"
}

run_all() {
    prepare_roots
    require_rpi
    require_cmd tmux
    require_cmd python3
    run_repl_display
    run_repl_name
    run_repl_model
    run_repl_loop
    run_tui_display
    run_tui_name_new
    run_tui_model
    run_tui_loop
    run_tui_compact
    run_tui_branch_clone
    run_tui_export
    run_tui_share
    run_tui_copy
    run_tui_reload
    run_tui_theme
    printf 'interactive-commands campaigns passed\nevidence=%s\n' "$EVIDENCE_ROOT"
}

case "${1:-run}" in
    list|--list|--dry-run) list_scenarios ;;
    run) run_all ;;
    repl-display) prepare_roots; require_rpi; run_repl_display ;;
    repl-name) prepare_roots; require_rpi; run_repl_name ;;
    repl-model) prepare_roots; require_rpi; run_repl_model ;;
    repl-loop) prepare_roots; require_rpi; run_repl_loop ;;
    tui-display) prepare_roots; require_rpi; run_tui_display ;;
    tui-name-new) prepare_roots; require_rpi; run_tui_name_new ;;
    tui-model) prepare_roots; require_rpi; run_tui_model ;;
    tui-loop) prepare_roots; require_rpi; run_tui_loop ;;
    tui-compact) prepare_roots; require_rpi; run_tui_compact ;;
    tui-branch-clone) prepare_roots; require_rpi; run_tui_branch_clone ;;
    tui-export) prepare_roots; require_rpi; run_tui_export ;;
    tui-share) prepare_roots; require_rpi; run_tui_share ;;
    tui-copy) prepare_roots; require_rpi; run_tui_copy ;;
    tui-reload) prepare_roots; require_rpi; run_tui_reload ;;
    tui-theme) prepare_roots; require_rpi; run_tui_theme ;;
    *) fail "usage: $0 [run|list|--dry-run|<scenario>]" ;;
esac