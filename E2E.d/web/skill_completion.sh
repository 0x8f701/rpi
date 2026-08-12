#!/usr/bin/env bash
# Focused Web /skill candidate regression lane (playwright-only hard gate).
#
# Spawns the real `rpi --listen` binary (--approve so the project `.pi` scope
# is trusted) and asserts the /skill picker contract in two phases:
#   with-skill: TWO fixture project skills are seeded under
#               `.pi/skills/greet/SKILL.md` and `.pi/skills/docs/SKILL.md`;
#               opening the picker -> /skill drills into skills mode and BOTH
#               candidates render as .command-picker__option rows with
#               data-skill-name + non-empty frontmatter descriptions — proving
#               candidates come from the REAL loaded disk catalog via
#               get_commands, not a hardcoded list. Selecting `greet` inserts
#               `/skill greet` (no auto-submit); Enter dispatches the `skill`
#               RPC and the summary bubble (div.msg.msg--summary, label
#               "skill") renders `name: greet` + its description.
#   no-skill: a fresh workspace with NO .pi/skills dir shows the
#             `.command-picker__hint` "No skills loaded" with zero candidate
#             rows after selecting /skill.
#
# Browser driver: playwright via npm (ephemeral install) with a system
# Chrome/Chromium binary or playwright's bundled chromium. Missing node, a
# failed playwright install, or no usable Chromium FAILS the lane (exit 1,
# setup); assertion failures exit 2+.
#
# Usage: bash E2E.d/web/skill_completion.sh [run|list]
set -euo pipefail
SCRIPT_DIR="$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
E2E_DIR="$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd -P)"
# shellcheck source=../lib/common.sh
. "$E2E_DIR/lib/common.sh"
# shellcheck source=lib/fixture.sh
. "$SCRIPT_DIR/lib/fixture.sh"

SCENARIO="web-skill-completion"
TOKEN="web-skill-completion-e2e-token-$$-$(date +%s)"
SKILL_NAME="greet"
SKILL_DESC="Greet skill for E2E"
SECOND_SKILL_NAME="docs"
SECOND_SKILL_DESC="Docs skill for E2E"

# spawn `rpi --listen` with --approve so the project skill is trusted. Mirrors
# commands_review.sh's spawn_rpi (web_spawn_rpi has no --approve knob).
# $1 root  $2 evidence  $3 mock port. -> sets RPI_PID via register_pid.
spawn_rpi() {
    local root="$1" evidence="$2" port="$3"
    local stdin_path="$root/stdin"
    rm -f "$stdin_path"
    mkfifo "$stdin_path"
    exec 9<>"$stdin_path"
    (
        cd "$root/workspace"
        exec env -i \
            HOME="$root/home" USERPROFILE="$root/home" \
            PATH="${PATH:-/usr/bin:/bin}" LANG="${LANG:-C.UTF-8}" LC_ALL="${LC_ALL:-C.UTF-8}" \
            PI_CODING_AGENT_DIR="$root/home/.pi/agent" \
            PI_OFFLINE=1 PI_SKIP_VERSION_CHECK=1 \
            RPI_WEB_DEV_DIR="${RPI_WEB_DEV_DIR:-}" \
            "$RPI_BIN" --offline --approve \
            --listen 127.0.0.1:0 \
            --listen-plaintext \
            --listen-token-file "$root/token" \
            --model user-steering/mock --api-key user-mock-key \
            <"$stdin_path" >"$evidence/rpi.stdout" 2>"$evidence/rpi.stderr"
    ) &
    register_pid $!
}

seed_skill() {
    local root="$1" name="$2" desc="$3"
    mkdir -p "$root/workspace/.pi/skills/$name"
    cat >"$root/workspace/.pi/skills/$name/SKILL.md" <<EOF
---
name: $name
description: $desc
---
# $name

Deterministic web e2e fixture skill body.
EOF
}

run_phase() {
    local phase="$1" root="$2" evidence="$3"
    printf '%s\n' "$TOKEN" >"$root/token"
    local port
    port="$(web_start_mock "$root" "$evidence")"
    spawn_rpi "$root" "$evidence" "$port"
    local url
    url="$(web_wait_for_listener "$evidence")"
    log "$SCENARIO ($phase): listener at $url"
    local pw_status=0
    web_run_playwright "$url" "$evidence" "$root/playwright-$phase" \
        "$SCRIPT_DIR/skill_completion_test.mjs" \
        "RPI_PHASE=$phase" \
        "RPI_SKILL_NAME=$SKILL_NAME" \
        "RPI_SKILL_DESC=$SKILL_DESC" \
        "RPI_SECOND_SKILL_NAME=$SECOND_SKILL_NAME" \
        "RPI_SECOND_SKILL_DESC=$SECOND_SKILL_DESC" || pw_status=$?
    web_kill_rpi
    web_kill_mock "$root"
    if [ "$pw_status" -eq 0 ]; then
        printf 'playwright (%s): skill completion contract PASSED\n' "$phase" | tee "$evidence/playwright-$phase-summary.txt"
    elif [ "$pw_status" -eq 1 ]; then
        fail "$SCENARIO ($phase): playwright SETUP FAILED (node/chromium/npm unavailable)"
    else
        fail "$SCENARIO ($phase): playwright lane failed (exit $pw_status)"
    fi
    web_sanity_http "$url" >/dev/null
}

main() {
    case "${1:-run}" in
        list|--list|--dry-run)
            printf '%s\n' \
                'web-skill-completion - /skill candidates come from the REAL loaded disk catalog: REAL page.click on /skill drills into skills mode with the Enter-to-run instruction and stays open, TWO seeded skills both render as .command-picker__option rows (data-skill-name + frontmatter desc), name/description search finds them, selecting greet inserts /skill greet selected + ready toast (no auto-submit) + Enter renders the frontmatter summary bubble, and a no-skill workspace shows the guided "No skills loaded" hint (load dirs + reload actions) with zero candidates (PLAYWRIGHT-ONLY, hard-fail)'
            return 0
            ;;
        run|all) ;;
        *) fail "usage: $0 [run|list]" ;;
    esac

    require_rpi
    web_require_browser

    # Phase 1: two fixture project skills loaded from disk.
    local root evidence
    root="$(scenario_workspace "$SCENARIO")"
    evidence="$EVIDENCE_ROOT/$SCENARIO"
    mkdir -p "$root/workspace"
    seed_skill "$root" "$SKILL_NAME" "$SKILL_DESC"
    seed_skill "$root" "$SECOND_SKILL_NAME" "$SECOND_SKILL_DESC"
    run_phase "with-skills" "$root" "$evidence"

    # Phase 2: no-skill empty state. Fresh workspace with NO .pi/skills dir so
    # get_commands projects zero skill candidates.
    local root2 evidence2
    root2="$(scenario_workspace "$SCENARIO-no-skill")"
    evidence2="$EVIDENCE_ROOT/$SCENARIO-no-skill"
    mkdir -p "$root2/workspace"
    run_phase "no-skill" "$root2" "$evidence2"

    web_finish_lane "$SCENARIO"
}

main "$@"
