#!/usr/bin/env bash
# Web code-review gap-expand regression (playwright-only hard gate).
#
# Isolated git fixture: 50-line file with a change around line 40 so a
# gap exists above the hunk. Opens /code-review on the real listener,
# selects the file, clicks Expand toward hunk (▼), and asserts the
# unique line above the hunk appears while hunk identity stays frozen.
# Desktop (≥1280) and mobile (390×844) screenshots are captured.
set -euo pipefail
SCRIPT_DIR="$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
E2E_DIR="$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd -P)"
. "$E2E_DIR/lib/common.sh"
. "$SCRIPT_DIR/lib/fixture.sh"

SCENARIO="web-code-review-expand"
TOKEN="web-code-review-expand-e2e-token-$$-$(date +%s)"
DIRTY_FILE="wide.rs"
EXPAND_MARKER="EXPAND-ABOVE-17 unique-gap-marker"
HUNK_MARKER="wide-line-40-EDIT"

seed_expand_fixture() {
    local workspace="$1"
    require_cmd git
    git -C "$workspace" init -q
    git -C "$workspace" config user.email "web-e2e@example.com"
    git -C "$workspace" config user.name "Web E2E"
    git -C "$workspace" config commit.gpgsign false
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
    case "${1:-run}" in
        list|--list|--dry-run)
            printf '%s\n' \
                'web-code-review-expand - gap above a hunk: ▼ Expand toward hunk loads snapshot context without changing hunk identity; 20-line clamp on the wire; desktop+mobile screenshots; zero browser errors (playwright, hard gate)'
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
    seed_expand_fixture "$root/workspace"
    web_spawn_rpi "$root" "$evidence" "$port"
    url="$(web_wait_for_listener "$evidence")"
    log "$SCENARIO: listener at $url"

    local pw_status=0
    web_run_playwright "$url" "$evidence" "$root/playwright" "$SCRIPT_DIR/code_review_expand_test.mjs" \
        RPI_DIRTY_FILE="$DIRTY_FILE" \
        RPI_EXPAND_MARKER="$EXPAND_MARKER" \
        RPI_HUNK_MARKER="$HUNK_MARKER" || pw_status=$?
    if [ "$pw_status" -eq 0 ]; then
        printf 'playwright: code-review gap expand (▼ toward hunk; marker above hunk; identity frozen; count<=20; desktop+mobile; zero browser errors): PASSED\n' \
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
