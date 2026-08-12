#!/usr/bin/env bash
# Focused personas-panel lane (playwright-only HARD GATE, fixture.sh-shared).
# Same fixture shape as the other hard-gate lanes (steering mock +
# orchestration enabled + real `rpi --listen`), plus a seeded durable persona
# (persona.md + memory + sessions) under the fixture agent dir. Covers the
# persistent-persona Web surface: list with memory/session counts, view
# definition, select as preferred, run (task_spawn with the persona agent
# name), create (catalog discoverable after the config save), edit
# name-agreement gate, remove-vs-purge confirmation dialog with the
# containment semantics verified on the REAL filesystem, and DOM hygiene (no
# credentials, no absolute paths).
#
# The lane reuses the shared fixture harness (web_require_browser /
# web_start_mock / web_spawn_rpi / web_wait_for_listener / web_run_playwright
# / web_sanity_http / web_finish_lane), so npm/chromium setup and the V8
# coverage-hook preload (RPI_COVERAGE_DIR/LANE) match every other lane.
#
# Usage: bash E2E.d/web/personas.sh [run|list]
set -euo pipefail
SCRIPT_DIR="$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
E2E_DIR="$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd -P)"
# shellcheck source=../lib/common.sh
. "$E2E_DIR/lib/common.sh"
# shellcheck source=lib/fixture.sh
. "$SCRIPT_DIR/lib/fixture.sh"

SCENARIO="web-personas"
TOKEN="web-personas-token-$$-$(date +%s)"

# Seed a durable persona with observable state: persona.md plus one memory
# entry and one session archive, so the panel's persistence counts and the
# remove/purge containment (verified on disk by the playwright test) are real.
seed_persona() {
    local root="$1" name="$2"
    local dir="$root/home/.pi/agent/personas/$name"
    mkdir -p "$dir/memory" "$dir/sessions"
    cat >"$dir/persona.md" <<EOF
---
name: $name
description: durable $name persona for web e2e
personality: calm
---
$name persona prompt for deterministic web e2e coverage.
EOF
    printf '%s\n' '{"id":"a","content":"persona-memory-note","tags":[],"ts":1,"session":"s"}' \
        >"$dir/memory/entries.jsonl"
    # A REAL native session archive (valid header) so the persona Run job's
    # continuity load succeeds — a `{}` stub would make the spawned job fail
    # with a continuity error.
    printf '%s\n' '{"type":"session","version":3,"id":"mentor-seed","timestamp":"2026-01-01T00:00:00.000Z","cwd":"."}' \
        >"$dir/sessions/Mentor.jsonl"
}

# Oversized definition (over the Web editor's 64 KiB editable bound, but
# under the core 256 KiB discovery bound) so the panel shows the read-only
# editor path. Carries real memory + a valid session archive so the Run
# button's Enter-key path can spawn it for real.
seed_big_persona() {
    local root="$1" name="$2"
    local dir="$root/home/.pi/agent/personas/$name"
    mkdir -p "$dir/memory" "$dir/sessions"
    {
        printf -- '---\nname: %s\ndescription: oversized persona definition for the read-only web editor\npersonality: patient\n---\n' "$name"
        i=1
        while [ "$i" -le 1000 ]; do
            printf 'line %04d: deterministic padding line for the oversized persona definition (web e2e).\n' "$i"
            i=$((i + 1))
        done
    } >"$dir/persona.md"
    printf '%s\n' '{"id":"a","content":"persona-memory-note","tags":[],"ts":1,"session":"s"}' \
        >"$dir/memory/entries.jsonl"
    printf '%s\n' '{"type":"session","version":3,"id":"big-seed","timestamp":"2026-01-01T00:00:00.000Z","cwd":"."}' \
        >"$dir/sessions/Big.jsonl"
}

# Minimal persona with NO memory/sessions state: the panel must show the
# zero-count persistence summary ("memory: 0 entries · sessions: 0 archives").
seed_lean_persona() {
    local root="$1" name="$2"
    local dir="$root/home/.pi/agent/personas/$name"
    mkdir -p "$dir"
    cat >"$dir/persona.md" <<EOF
---
name: $name
description: lean persona with no durable state yet
---
$name persona prompt for deterministic web e2e coverage.
EOF
}

# Persona whose local memory is a SYMLINK: state counting must fail closed
# and the panel shows the "memory/session state unreadable" literal (never a
# path).
seed_ghost_persona() {
    local root="$1" name="$2"
    local dir="$root/home/.pi/agent/personas/$name"
    mkdir -p "$dir/memory"
    cat >"$dir/persona.md" <<EOF
---
name: $name
description: persona with unreadable memory state
---
$name persona prompt for deterministic web e2e coverage.
EOF
    ln -s "/nonexistent/$name-memory-target" "$dir/memory/entries.jsonl"
}

main() {
    case "${1:-run}" in
        list|--list|--dry-run)
            printf '%s\n' \
                'web-personas - Personas panel: list/counts/view/select/run/create/edit-name-gate/remove-vs-purge confirm + on-disk containment + DOM hygiene (playwright hard gate)'
            return 0
            ;;
        run|all) ;;
        *) fail "usage: $0 [run|list]" ;;
    esac

    require_rpi
    # Hard gate per the Web suite contract: node is REQUIRED, and
    # web_run_playwright fails (setup, exit 1) on a missing npm, a failed
    # playwright install, or no usable Chromium — never a skip.
    web_require_browser

    local root evidence port url
    root="$(scenario_workspace "$SCENARIO")"
    evidence="$EVIDENCE_ROOT/$SCENARIO"
    printf '%s\n' "$TOKEN" >"$root/token"

    port="$(web_start_mock "$root" "$evidence")"
    seed_persona "$root" "mentor"
    seed_big_persona "$root" "big"
    seed_lean_persona "$root" "lean"
    seed_ghost_persona "$root" "ghost"
    # Orchestration must be enabled for task_spawn (the Run button path).
    cat >"$root/home/.pi/agent/settings.json" <<EOF
{
  "orchestration": {
    "tasks": true,
    "maxConcurrency": 2,
    "maxRecursionDepth": 2
  }
}
EOF

    web_spawn_rpi "$root" "$evidence" "$port"
    url="$(web_wait_for_listener "$evidence")"
    log "$SCENARIO: listener at $url"

    local pw_status=0
    web_run_playwright "$url" "$evidence" "$root/playwright" \
        "$SCRIPT_DIR/personas_test.mjs" \
        RPI_PERSONA_ROOT="$root/home/.pi/agent/personas" || pw_status=$?
    if [ "$pw_status" -eq 0 ]; then
        printf 'playwright: personas panel contract: PASSED\n' | tee "$evidence/playwright-summary.txt"
    elif [ "$pw_status" -eq 1 ]; then
        fail "$SCENARIO: playwright SETUP FAILED (node/chromium/npm unavailable)"
    else
        fail "$SCENARIO: playwright lane failed (exit $pw_status)"
    fi

    web_sanity_http "$url"
    web_finish_lane "$SCENARIO"
}

main "$@"
