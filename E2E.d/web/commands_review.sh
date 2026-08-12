#!/usr/bin/env bash
# Web composer command-button + code-review panel E2E lane
# (playwright-only hard gate).
#
# Spawns the real `rpi --listen` binary with a token file and the loopback
# mock provider (--scenario steering), opens `/web` in a real browser, and
# asserts the v0.2.10 Web composer command surface + the real code-review
# panel against a seeded TEMP git repo + a fixture project skill:
#   - #command-btn sits LEFT of #prompt-input in the composer row
#   - opening the command picker shows /compact, /skill, /code-review
#     (the backend get_commands catalog is authoritative)
#   - choosing /code-review inserts the draft into the composer WITHOUT
#     auto-submitting (no user bubble, panel still closed)
#   - Enter dispatches code_review_open and the real review panel renders
#     the HEAD→working-tree comparison label, the dirty file, and the
#     changed diff lines; explicit hunk selection (never auto-selected),
#     the file filter, keyboard file navigation, the truncated fixture
#     banner, Ctrl+Enter comment submit + streaming/abort, the inline
#     close-confirm guard, Escape close, mobile Files/Diff/Thread tab
#     transitions, and closing the panel removes it from the DOM
#   - the dirty file carries TWO separated diff hunks; a comment on the
#     SECOND hunk lands only in that hunk's thread (identity-keyed), the
#     per-hunk drafts A/B survive hunk switches, and submitting A leaves B
#   - TUI/Web parity for the changed-file tree: compact colored status
#     glyphs (M/A), basename-only rows with the full repo-relative path in
#     data-file-path/title and the readable full state in aria-label,
#     selection/filter keyed on the full path, and no rail overflow on
#     desktop or mobile (stats never swallow the filename)
#   - switching files clears the composer/hunk selection
#   - hostile diff/comment text (`<script>` + `<img onerror>` markers) stays
#     literal text — no element is created and no script runs (no dialog)
#   - the panel polls code_review_snapshot at the 1.5s contract while open
#     (≥2 frames ~1.5s apart observed) and polling stops after panel close
#   - a session switch closes the owning review workspace via a stamped
#     code_review_close (sessionId A), and a bare /code-review in the target
#     session never reuses the previous session's revision args
#   - /skill <fixture> renders the loaded skill's frontmatter summary visibly
#   - /compact dispatches the compact RPC — observed on the outgoing WS
#     frame (deterministic; the provider round-trip is NOT required for the
#     assertion, so an empty session is fine and the lane stays fast)
#
# The fixture workspace is a TEMP git repo (loopback only; never the real
# repository) with a committed baseline, a dirty tracked file carrying TWO
# separated diff hunks (additions + deletions, including a hostile
# `<script>`/`<img onerror>` literal line), a staged new file, an oversize
# (>4000 diff lines) file the backend marks truncated, and a project skill
# under `.pi/skills/greet/SKILL.md`. The listener is launched with
# `--approve` so the project skill is trusted and resolvable by the
# stateless `skill` RPC.
#
# Browser driver: playwright via npm (ephemeral install in the scenario work
# dir) with a system Chrome/Chromium binary or playwright's bundled chromium.
# Missing node, a failed playwright install, or no usable Chromium FAILS the
# lane (exit 1, setup); assertion failures exit 2+.
#
# Usage: bash E2E.d/web/commands_review.sh [run|list]
set -euo pipefail
SCRIPT_DIR="$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
E2E_DIR="$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd -P)"
# shellcheck source=../lib/common.sh
. "$E2E_DIR/lib/common.sh"
# shellcheck source=lib/fixture.sh
. "$SCRIPT_DIR/lib/fixture.sh"

SCENARIO="web-commands-review"
TOKEN="web-commands-review-e2e-token-$$-$(date +%s)"
# Fixture file + skill values the playwright half asserts against (kept in
# lockstep with commands_review_test.mjs).
DIRTY_FILE="greet.txt"
ADDED_FILE="added.txt"
BIG_FILE="big.txt"
NESTED_FILE="nested/deep.txt"
SKILL_NAME="greet"
SKILL_DESC="Greet skill for E2E"

# ---------------------------------------------------------------------------
# spawn `rpi --listen` with --approve so the project skill is trusted. This
# mirrors core.sh's self-contained spawn (web_spawn_rpi has no --approve knob
# and the standalone lanes must not edit the shared fixture helper to add one).
# $1 root  $2 evidence  $3 mock port. -> sets RPI_PID via register_pid.
# ---------------------------------------------------------------------------
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

# ---------------------------------------------------------------------------
main() {
    case "${1:-run}" in
        list|--list|--dry-run)
            printf '%s\n' \
                'web-commands-review - composer command button left of textarea, command picker lists /compact /skill /code-review, /code-review draft + no auto-submit, Enter opens real review panel (HEAD→working tree + dirty file + changed lines), explicit hunk selection + file filter + keyboard nav + truncated banner + Ctrl+Enter comment/stream/abort + inline close confirm + Escape close + mobile Files/Diff/Thread tabs, TUI/Web parity (M/A compact glyphs, basename-only rows with full path/state in data/title/aria, no rail overflow desktop + mobile), two separated hunks with second-hunk thread ownership + per-hunk drafts A/B + file switch clears selection, hostile diff/comment stays literal (no dialog/script side effect), 1.5s snapshot polling observed and stops on close, session switch sends stamped code_review_close + no stale rev args, /skill <fixture> visible summary, /compact outgoing-WS dispatch (playwright, hard gate)'
            return 0
            ;;
        run|all) ;;
        *) fail "usage: $0 [run|list]" ;;
    esac

    require_rpi
    web_require_browser
    require_cmd git

    local root evidence port url
    root="$(scenario_workspace "$SCENARIO")"
    evidence="$EVIDENCE_ROOT/$SCENARIO"
    printf '%s\n' "$TOKEN" >"$root/token"

    port="$(web_start_mock "$root" "$evidence")"

    # Seed a TEMP git repo (loopback fixture only; never the real repo) with a
    # committed baseline, then a dirty tracked file carrying TWO separated
    # diff hunks (including a hostile literal line), a staged new file, and
    # an oversize file so HEAD→working-tree code review has real content.
    git -C "$root/workspace" init -q
    git -C "$root/workspace" config user.email "web-e2e@example.com"
    git -C "$root/workspace" config user.name "Web E2E"
    git -C "$root/workspace" config commit.gpgsign false
    # Baseline: 18 lines with the two change regions >6 untouched lines apart
    # (git's default 3-line context), so the dirty tree produces TWO separated
    # hunks — @@ -1,6 +1,7 @@ (beta→BETA + insert delta) and
    # @@ -13,6 +14,7 @@ (kappa→KAPPA + hostile insert) — never one merged hunk.
    cat >"$root/workspace/$DIRTY_FILE" <<'EOF'
alpha
beta
gamma
epsilon
zeta
eta
theta
iota
one
two
three
four
five
six
seven
kappa
lambda
mu
EOF
    # Nested fixture file so the changed-file rail renders a collapsible
    # directory tree (nested/ -> deep.txt), exercised by the tree assertions.
    mkdir -p "$root/workspace/nested"
    cat >"$root/workspace/nested/deep.txt" <<'EOF'
deep-one
deep-two
deep-three
EOF
    git -C "$root/workspace" add -- "$DIRTY_FILE" nested/deep.txt
    git -C "$root/workspace" -c commit.gpgsign=false commit -q -m baseline
    # Dirty working tree, two SEPARATED hunks. The second hunk inserts a
    # hostile line (`<script>` + `<img onerror>`) that the diff pipeline must
    # treat as opaque text: the panel renders it literally (React text, never
    # innerHTML) and it must create no element and run no script.
    cat >"$root/workspace/$DIRTY_FILE" <<'EOF'
alpha
BETA
gamma
delta
epsilon
zeta
eta
theta
iota
one
two
three
four
five
six
seven
KAPPA
<script>window.__crPwned=1</script><img src=x onerror=window.__crPwned=2>
lambda
mu
EOF
    # Staged new tracked file (HEAD→working-tree review shows staged + unstaged).
    printf 'new file content\n' >"$root/workspace/$ADDED_FILE"
    git -C "$root/workspace" add -- "$ADDED_FILE"
    # Dirty the nested fixture file (deep-two -> DEEP-TWO) so the tree has a
    # real nested directory node with one changed file beneath it.
    cat >"$root/workspace/nested/deep.txt" <<'EOF'
deep-one
DEEP-TWO
deep-three
EOF
    # Oversize file: > MAX_FILE_RENDER_LINES (4000) rendered diff lines so the
    # backend marks the FILE truncated (deterministic truncated-state surface;
    # the panel must show the truncated token and banner). 4100 changed lines
    # => ~8200 diff lines, still far under the 2 MiB snapshot cap so only the
    # per-file truncation fires.
    BIG_LINES=4100
    for i in $(seq 1 "$BIG_LINES"); do printf 'base-line-%05d\n' "$i"; done >"$root/workspace/$BIG_FILE"
    git -C "$root/workspace" add -- "$BIG_FILE"
    # Pathspec-scoped commit: added.txt must stay staged-but-uncommitted so
    # HEAD→working-tree review still shows it as a new file.
    git -C "$root/workspace" -c commit.gpgsign=false commit -q -m "big baseline" -- "$BIG_FILE"
    for i in $(seq 1 "$BIG_LINES"); do printf 'changed-line-%05d\n' "$i"; done >"$root/workspace/$BIG_FILE"

    # Fixture project skill so /skill <name> resolves to a loaded frontmatter
    # summary. --approve (spawn_rpi) trusts the project `.pi` scope.
    mkdir -p "$root/workspace/.pi/skills/$SKILL_NAME"
    cat >"$root/workspace/.pi/skills/$SKILL_NAME/SKILL.md" <<EOF
---
name: $SKILL_NAME
description: $SKILL_DESC
---
# $SKILL_NAME

Deterministic web e2e fixture skill body.
EOF

    spawn_rpi "$root" "$evidence" "$port"
    url="$(web_wait_for_listener "$evidence")"
    log "$SCENARIO: listener at $url"

    local pw_status=0
    web_run_playwright "$url" "$evidence" "$root/playwright" \
        "$SCRIPT_DIR/commands_review_test.mjs" \
        RPI_DIRTY_FILE="$DIRTY_FILE" \
        RPI_ADDED_FILE="$ADDED_FILE" \
        RPI_BIG_FILE="$BIG_FILE" \
        RPI_NESTED_FILE="$NESTED_FILE" \
        RPI_SKILL_NAME="$SKILL_NAME" \
        RPI_SKILL_DESC="$SKILL_DESC" || pw_status=$?
    if [ "$pw_status" -eq 0 ]; then
        printf 'playwright: command button + picker + /code-review panel + /skill summary + /compact dispatch: PASSED\n' | tee "$evidence/playwright-summary.txt"
    elif [ "$pw_status" -eq 1 ]; then
        fail "$SCENARIO: playwright SETUP FAILED (node/chromium/npm unavailable)"
    else
        fail "$SCENARIO: playwright lane failed (exit $pw_status)"
    fi

    web_sanity_http "$url"
    web_finish_lane "$SCENARIO"
}

main "$@"