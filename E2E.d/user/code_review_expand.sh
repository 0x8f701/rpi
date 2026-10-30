#!/usr/bin/env bash
# User-perspective TUI code-review gap expand: a 50-line dirty file with a
# change around line 40 must show an expand row above the hunk; focusing the
# diff and pressing `e` reveals the unique line-17 marker without dropping
# the hunk edit.
set -euo pipefail
SCRIPT_DIR="$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
E2E_DIR="$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd -P)"
. "$E2E_DIR/lib/common.sh"
. "$E2E_DIR/lib/orchestration_fixtures.sh"

SCENARIO="code-review-expand"
DIRTY_FILE="wide.rs"
EXPAND_MARKER="EXPAND-ABOVE-17 unique-gap-marker"
HUNK_MARKER="wide-line-40-EDIT"
FAUX_REPLY="deterministic-code-review-expand-reply"

wait_for() {
    local needle="$1" timeout="${2:-25}"
    if ! tmux_wait_for "$session" "$timeout" "$needle" >"$evidence/live.txt"; then
        fail "user.$SCENARIO: did not find ${needle@Q} (see $evidence/live.txt)"
    fi
}

seed_expand_fixture() {
    local workspace="$1"
    require_cmd git
    git -C "$workspace" init -q
    git -C "$workspace" config user.email "tui-e2e@example.com"
    git -C "$workspace" config user.name "TUI E2E"
    git -C "$workspace" config commit.gpgsign false
    require_cmd python3
    python3 - "$workspace/$DIRTY_FILE" "$EXPAND_MARKER" <<'PY'
import sys
path, marker = sys.argv[1], sys.argv[2]
lines = []
for n in range(1, 51):
    if n == 17:
        lines.append(f"{marker}\n")
    else:
        lines.append(f"wide-line-{n:02d}\n")
open(path, "w", encoding="utf-8").writelines(lines)
PY
    git -C "$workspace" add -- "$DIRTY_FILE"
    git -C "$workspace" -c commit.gpgsign=false commit -q -m baseline
    python3 - "$workspace/$DIRTY_FILE" "$HUNK_MARKER" <<'PY'
import sys
path, hunk = sys.argv[1], sys.argv[2]
text = open(path, encoding="utf-8").read().replace("wide-line-40\n", hunk + "\n")
open(path, "w", encoding="utf-8").write(text)
PY
}

main() {
    require_rpi
    require_cmd tmux
    require_cmd git

    local root evidence session
    root="$(scenario_workspace "$SCENARIO")"
    evidence="$EVIDENCE_ROOT/$SCENARIO"
    session="$(unique_tmux_name "$SCENARIO")"
    register_tmux_session "$session"
    seed_expand_fixture "$root/workspace"

    tmux new-session -d -s "$session" -x 140 -y 42 -c "$root/workspace" \
        "env HOME='$root/home' USERPROFILE='$root/home' \
PI_CODING_AGENT_DIR='$root/home/.pi/agent' \
RPI_OFFLINE=1 RPI_SKIP_VERSION_CHECK=1 \
PI_FAUX_RESPONSE='$FAUX_REPLY' \
TERM=xterm-256color \
'$RPI_BIN' --offline --model faux/faux-1; printf '===TUI-DONE-code-review-expand===\n'"

    tmux_wait_for "$session" 25 'faux/faux-1' >"$evidence/boot.txt" || true
    tmux send-keys -t "$session":0 Escape
    sleep 0.3

    tmux send-keys -t "$session":0 -l '/code-review'
    tmux send-keys -t "$session":0 Enter
    wait_for 'Code review' 30
    wait_for "$DIRTY_FILE" 30
    wait_for 'e expand' 30
    tmux capture-pane -p -S -2000 -t "$session":0 >"$evidence/01-before-expand.txt"
    if grep -F -q "$EXPAND_MARKER" "$evidence/01-before-expand.txt"; then
        fail "user.$SCENARIO: line-17 marker already visible before e (see $evidence/01-before-expand.txt)"
    fi
    if ! grep -F -q "$HUNK_MARKER" "$evidence/01-before-expand.txt"; then
        fail "user.$SCENARIO: hunk edit missing before expand (see $evidence/01-before-expand.txt)"
    fi

    tmux send-keys -t "$session":0 Tab
    sleep 0.2
    tmux send-keys -t "$session":0 -l 'e'
    if ! tmux_wait_for "$session" 20 "$EXPAND_MARKER" >"$evidence/02-after-expand.txt"; then
        tmux send-keys -t "$session":0 Home
        if ! tmux_wait_for "$session" 10 "$EXPAND_MARKER" >"$evidence/02-after-expand.txt"; then
            fail "user.$SCENARIO: e did not reveal ${EXPAND_MARKER@Q} (see $evidence/02-after-expand.txt)"
        fi
    fi
    if ! grep -F -q "$HUNK_MARKER" "$evidence/02-after-expand.txt"; then
        fail "user.$SCENARIO: hunk edit disappeared after expand (see $evidence/02-after-expand.txt)"
    fi
    if ! grep -F -q '@@' "$evidence/02-after-expand.txt"; then
        fail "user.$SCENARIO: hunk header disappeared after expand (see $evidence/02-after-expand.txt)"
    fi

    tmux send-keys -t "$session":0 q
    printf 'user.%s passed\nevidence=%s\n' "$SCENARIO" "$evidence"
}

main "$@"
