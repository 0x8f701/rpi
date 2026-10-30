#!/usr/bin/env bash
# Web code-review frozen-submodule lane (playwright-only hard gate).
# Fixture: review-base tag, then dirty ext (expands to ext/lib.txt) + clean
# file-changing SHA bump of sib-clean (checkout clean, parent gitlink kept).
# Zero comment submits; registry edits (LANES/README) reported to Main, not made.
# Usage: bash E2E.d/web/code_review_submodule.sh [run|list]
set -euo pipefail
SCRIPT_DIR="$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
E2E_DIR="$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd -P)"
. "$E2E_DIR/lib/common.sh"
. "$SCRIPT_DIR/lib/fixture.sh"

SCENARIO="web-code-review-submodule"
TOKEN="web-code-review-submodule-e2e-token-$$-$(date +%s)"
BASE_REF="review-base"
DIRTY_INNER="ext/lib.txt"
CLEAN_SIB="sib-clean"
CLEAN_SIB_INNER="sib-clean/file.txt"
MARKER="SUBMOD-P0-CHANGED-$$-$(date +%s)"

seed_submodule_fixture() {
    local workspace="$1" marker="$2"
    require_cmd git
    git -C "$workspace" init -q -b main
    git -C "$workspace" config user.email "web-e2e@example.com"
    git -C "$workspace" config user.name "Web E2E"
    git -C "$workspace" config commit.gpgsign false
    mkdir -p "$workspace/ext"
    for i in $(seq 1 400); do printf 'ext-line-%03d\n' "$i"; done >"$workspace/ext/lib.txt"
    git -C "$workspace/ext" init -q -b main
    git -C "$workspace/ext" config user.email "web-e2e@example.com"
    git -C "$workspace/ext" config user.name "Web E2E"
    git -C "$workspace/ext" config commit.gpgsign false
    git -C "$workspace/ext" add lib.txt
    git -C "$workspace/ext" -c commit.gpgsign=false commit -q -m "ext initial"
    mkdir -p "$workspace/sib-clean"
    printf 'sibling file\n' >"$workspace/sib-clean/file.txt"
    git -C "$workspace/sib-clean" init -q -b main
    git -C "$workspace/sib-clean" config user.email "web-e2e@example.com"
    git -C "$workspace/sib-clean" config user.name "Web E2E"
    git -C "$workspace/sib-clean" config commit.gpgsign false
    git -C "$workspace/sib-clean" add file.txt
    git -C "$workspace/sib-clean" -c commit.gpgsign=false commit -q -m "sib initial"
    git -C "$workspace" -c protocol.file.allow=always submodule add -q "$workspace/ext" ext
    git -C "$workspace" -c protocol.file.allow=always submodule add -q "$workspace/sib-clean" sib-clean
    git -C "$workspace" add .
    git -C "$workspace" -c commit.gpgsign=false commit -q -m "record submodules"
    git -C "$workspace" tag "$BASE_REF"
    git -C "$workspace" rev-parse --verify -q "$BASE_REF^{commit}" >/dev/null ||
        fail "review-base tag did not resolve to a commit"
    git -C "$workspace" ls-files -s -- ext sib-clean | awk '$1 == "160000" { n++ } END { exit n == 2 ? 0 : 1 }' ||
        fail "fixture: ext and sib-clean must both be mode-160000 gitlinks"
    git -C "$workspace/ext" -c commit.gpgsign=false commit --allow-empty -q -m "ext bump"
    for n in 390 375 360 345 330 315 300 285 270 255 240 210 180 150 120 90 60 30; do
        sed -i "${n}d" "$workspace/ext/lib.txt"
    done
    printf '%s\n' "$marker" >>"$workspace/$DIRTY_INNER"
    printf 'sibling file\nSIB-CLEAN-COMMITTED-BUMP\n' >"$workspace/sib-clean/file.txt"
    git -C "$workspace/sib-clean" add file.txt
    git -C "$workspace/sib-clean" -c commit.gpgsign=false commit -q -m "sib bump"
    grep -Fq "$marker" "$workspace/$DIRTY_INNER" ||
        fail "fixture: marker missing from dirty inner file"
    grep -Fq 'SIB-CLEAN-COMMITTED-BUMP' "$workspace/$CLEAN_SIB_INNER" ||
        fail "fixture: committed sib-clean child change missing"
    if git -C "$workspace/sib-clean" diff --quiet HEAD~1; then
        fail "fixture: sib-clean bump did not change a tracked file"
    fi
    if git -C "$workspace" diff --quiet "$BASE_REF" -- ext; then
        fail "fixture: review-base already records the ext bump (tag created after the edits)"
    fi
    if git -C "$workspace" diff --quiet "$BASE_REF" -- sib-clean; then
        fail "fixture: review-base already records the sib-clean bump (tag created after the edits)"
    fi
    [ -z "$(git -C "$workspace/sib-clean" status --porcelain)" ] ||
        fail "fixture: sib-clean working tree is not clean (inner diff would expand it)"
    local expected actual
    expected="$(printf ' M ext\n M sib-clean\n')"
    actual="$(git -C "$workspace" status --porcelain)"
    [ "$actual" = "$expected" ] ||
        fail "fixture: parent status must be exactly two submodule modifications: $(printf '%q' "$actual")"
}

main() {
    case "${1:-run}" in
        list|--list|--dry-run)
            printf '%s\n' \
                'web-code-review-submodule - /code-review review-base over a frozen no-network fixture: exactly one code_review_open (from=review-base includeWorkingTree=true, no to), expanded dirty submodule row ext/lib.txt with the SUBMOD-P0 marker as the single addition (visible tinted background vs the hunk card) and NO bare ext row, clean file-changing SHA bump of sib-clean kept as modified gitlink (sib-clean/file.txt not expanded), enabled comment composer (typed value echoed, never submitted, zero code_review_comment submits enforced), desktop 1280x800 three-pane Files/Diff/Hunk comments with no tab bar + real scroll and mobile 390x844 Files/Diff/Thread tab navigation with no horizontal overflow, desktop+mobile screenshots, zero browser errors (playwright, hard gate)'
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
    seed_submodule_fixture "$root/workspace" "$MARKER"
    web_spawn_rpi "$root" "$evidence" "$port"
    url="$(web_wait_for_listener "$evidence")"
    log "$SCENARIO: listener at $url"

    local pw_status=0
    web_run_playwright "$url" "$evidence" "$root/playwright" "$SCRIPT_DIR/code_review_submodule_test.mjs" \
        RPI_BASE_REF="$BASE_REF" \
        RPI_DIRTY_INNER="$DIRTY_INNER" \
        RPI_CLEAN_SIB="$CLEAN_SIB" \
        RPI_CLEAN_SIB_INNER="$CLEAN_SIB_INNER" \
        RPI_MARKER="$MARKER" || pw_status=$?
    if [ "$pw_status" -eq 0 ]; then
        printf 'playwright: code-review frozen submodule journey (exactly one open frame from=review-base; expanded ext/lib.txt with one SUBMOD-P0 addition + tinted row; no bare ext row; clean file-changing SHA bump of sib-clean kept modified gitlink, child path not expanded; enabled composer; zero comment submits; desktop three-pane + mobile tabs/scroll/overflow; zero browser errors): PASSED\n' \
            | tee "$evidence/playwright-summary.txt"
    elif [ "$pw_status" -eq 1 ]; then
        fail "$SCENARIO: playwright SETUP FAILED (node/chromium/npm unavailable)"
    else
        fail "$SCENARIO: playwright lane failed (exit $pw_status)"
    fi

    web_sanity_http "$url"
    web_finish_lane "$SCENARIO"
}

main "$@"
