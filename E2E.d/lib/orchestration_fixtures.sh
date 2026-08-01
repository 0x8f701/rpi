#!/usr/bin/env bash
# Shared fixtures for deterministic orchestration/TUI campaigns.
# shellcheck shell=bash
# Sourced by E2E.d/ci scripts after common.sh.

prepare_orchestration_settings() {
    local home="$1"
    mkdir -p "$home/.pi/agent"
    cat >"$home/.pi/agent/settings.json" <<'EOF'
{
  "orchestration": {
    "process": true,
    "tasks": true,
    "todo": true,
    "maxConcurrency": 4,
    "maxRecursionDepth": 2
  },
  "selector": {
    "autoSelectThreshold": 0
  }
}
EOF
}

# Trusted user-scope researcher/writer agents + overlapping research skill.
# Exact NL "Have researcher study this" must select researcher; skill-only
# "Use research for this" must not spawn.
prepare_orchestration_agents() {
    local home="$1"
    local agents="$home/.pi/agent/agents"
    local skill_dir="$home/.pi/agent/skills/research"
    mkdir -p "$agents" "$skill_dir"

    cat >"$agents/researcher.md" <<'EOF'
---
name: researcher
description: Research and study assigned topics
tools:
  - read
  - grep
  - hub
---
You are the researcher agent for deterministic E2E coverage.
EOF

    cat >"$agents/writer.md" <<'EOF'
---
name: writer
description: Write assigned content
tools:
  - read
  - write
  - hub
---
You are the writer agent for deterministic E2E coverage.
EOF

    cat >"$skill_dir/SKILL.md" <<'EOF'
---
name: research
description: Research topics for a researcher study
---
RESEARCH_BODY for skill-only routing checks.
EOF
}

prepare_orchestration_home() {
    local home="$1"
    prepare_orchestration_settings "$home"
    prepare_orchestration_agents "$home"
}

# Tiny valid PNG used only when a desktop clipboard injector is available.
# Prefer cargo image-placeholder tests when clipboard injection is unavailable.
write_orchestration_png_fixture() {
    local path="$1"
    python3 - "$path" <<'PY'
import base64
import sys
from pathlib import Path
# Minimal valid 1x1 PNG; label contract is [Image #N, WIDTHxHEIGHT].
png = base64.b64decode(
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg=="
)
Path(sys.argv[1]).write_bytes(png)
PY
}

# Wait until a tmux pane capture contains all needles (or fail).
tmux_wait_for() {
    local session="$1"
    local timeout_seconds="$2"
    shift 2
    local deadline needle capture missing
    deadline=$((SECONDS + timeout_seconds))
    while ((SECONDS < deadline)); do
        capture="$(tmux capture-pane -p -S -2000 -t "$session":0 2>/dev/null || true)"
        missing=0
        for needle in "$@"; do
            case "$capture" in
                *"$needle"*) ;;
                *) missing=1; break ;;
            esac
        done
        if [ "$missing" -eq 0 ]; then
            printf '%s\n' "$capture"
            return 0
        fi
        sleep 0.25
    done
    tmux capture-pane -p -S -2000 -t "$session":0 2>/dev/null || true
    return 1
}

# Prove that the ordinary composer remains visible and editable without relying
# on a resize-triggered repaint. The caller supplies a unique unsent draft.
assert_tmux_composer_editable() {
    local session="$1" evidence="$2" sentinel="$3" capture
    tmux send-keys -t "$session":0 -l "$sentinel"
    sleep 0.4
    tmux capture-pane -p -t "$session":0 >"$evidence"
    assert_file_contains "$evidence" "$sentinel"
    tmux send-keys -t "$session":0 -l 'X'
    sleep 0.25
    capture="$(tmux capture-pane -p -t "$session":0 2>/dev/null || true)"
    case "$capture" in
        *"${sentinel}X"*) ;;
        *) printf '%s\n' "$capture" >"$evidence"; fail "composer draft disappeared or stopped accepting input: $sentinel" ;;
    esac
    printf '%s\n' "$capture" >"$evidence"
    tmux send-keys -t "$session":0 C-u
    sleep 0.15
}

assert_file_contains() {
    local path="$1"
    shift
    local needle
    for needle in "$@"; do
        grep -F -- "$needle" "$path" >/dev/null || fail "expected ${needle@Q} in $path"
    done
}

assert_file_lacks() {
    local path="$1"
    shift
    local needle
    for needle in "$@"; do
        if grep -F -- "$needle" "$path" >/dev/null; then
            fail "did not expect ${needle@Q} in $path"
        fi
    done
}
