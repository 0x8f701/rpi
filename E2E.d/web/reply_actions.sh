#!/usr/bin/env bash
# Web reply-actions E2E lane (playwright-only hard gate).
#
# Frozen UI interface (Main contract):
#   - settled main-transcript assistant rows (.msg--assistant[data-entry-id])
#     carry EXACTLY THREE buttons stacked VERTICALLY, top-left: favorite
#     (button.assistant-star-toggle, class preserved) topmost, then
#     copy-source (button[data-reply-action="copy-markdown"]), then save.md
#     (button[data-reply-action="save-markdown"]) — one shared x column, y
#     strictly increasing star->copy->save (contract scope = the row's
#     reply-action group .msg--assistant__actions ONLY; per-fence copy
#     buttons inside the markdown body are separate working controls, out of
#     scope and left fully functional);
#   - three buttons ALWAYS visible (no hover dependency; >=44px touch on the
#     390x844 touch context); the census scrollIntoViews the real row before
#     any hit probe (an offscreen row makes elementFromPoint lie);
#   - copy = RAW markdown (text blocks concatenated, no trim, no metadata),
#     byte-exact (redaction is acceptable for a public reply source; the mock
#     constants carry no secret-shaped text, so byte-exactness holds either
#     way);
#   - save = UTF-8 download bytes byte-exact with the raw markdown; filename
#     = summary digest (<=30 unicode points, no language requirement,
#     path-illegal/control chars stripped) + the STABLE reply timestamp in
#     local YYYYMMDD-HHmmss — two saves on one reply, a reload, and the phone
#     surface saving the same reply all produce the SAME filename;
#   - clipboard FAILURE surfaces the app's central error toast (#toasts child
#     with toast--error / role=alert) — no invented flash surface;
#   - blob download: one createObjectURL + one revokeObjectURL per save with
#     the identical URL.
#
# The /chat SessionView surface is part of THIS feature and is a HARD
# assertion: the FULL three-button interface (star via the SHARED reply-star
# store + copy-markdown + save-markdown) must exist there stacked VERTICALLY
# (runner = the SAME listener, invited via the reviewed chat_mermaid journey);
# absence is a lane FAILURE, never a PENDING pass. Only a temporary live row
# without an entry id may omit the star; a final hydrated row carries all
# three. The bootstrap's single expected auth console error is the only
# tolerated non-fatal category (exact match, counted).
#
# Empty-reply handling is a PURE MODULE-BOUNDARY contract (the filename
# builder's empty-input fallback, owned by the UI owner's unit test) — not an
# E2E leg and never a PENDING claim.
#
# Rich fixture: the EXISTING mock constants RICH_TEXT / RICH_TEXT_EXTRA in
# E2E.d/lib/user_mock_server.py (routed by EXACT prompt text). The lane
# extracts them via importlib at fixture time so the expected bytes and the
# served reply share ONE source — clipboard read === raw; download UTF-8 ===
# raw, with no hand-copied constant that could drift.
#
# Browser driver: playwright via npm (ephemeral install in the scenario work
# dir) with a system Chrome/Chromium binary or playwright's bundled chromium.
# Missing node, a failed playwright install, or no usable Chromium FAILS the
# lane (exit 1, setup); assertion failures exit 2+. Viewport emulation is
# playwright-only, so there is no non-playwright fallback.
#
# Usage: bash E2E.d/web/reply_actions.sh [run|list]
set -euo pipefail
SCRIPT_DIR="$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
E2E_DIR="$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd -P)"
# shellcheck source=../lib/common.sh
. "$E2E_DIR/lib/common.sh"
# shellcheck source=lib/fixture.sh
. "$SCRIPT_DIR/lib/fixture.sh"

SCENARIO="web-reply-actions"
TOKEN="web-reply-actions-e2e-$$-$(date +%s)"

# Extract the mock's OWN rich constants as the byte-exact expectations.
write_expected_payloads() { # evidence-dir
    python3 - "$1" "$E2E_DIR/lib/user_mock_server.py" <<'PY'
import importlib.util
import pathlib
import sys
out = pathlib.Path(sys.argv[1])
mock = pathlib.Path(sys.argv[2])
spec = importlib.util.spec_from_file_location("user_mock_server_extract", mock)
mod = importlib.util.module_from_spec(spec)
spec.loader.exec_module(mod)
for name, filename in (("RICH_TEXT", "expected-rich.md"), ("RICH_TEXT_EXTRA", "expected-rich-extra.md")):
    value = getattr(mod, name, None)
    if not isinstance(value, str) or value == "":
        sys.exit(f"web-reply-actions: mock constant {name} missing/empty — refusing to author expectations")
    (out / filename).write_bytes(value.encode("utf-8"))
PY
}

main() {
    case "${1:-run}" in
        list|--list|--dry-run)
            printf '%s\n' \
                'web-reply-actions - reply row favorite/copy-markdown/save.md: vertical three-button stack (star topmost), byte-exact clipboard + UTF-8 download of the EXISTING rich mock constants, stable filename (summary<=30 unicode points + local YYYYMMDD-HHmmss) across two saves + reload + phone, always-visible >=44px touch targets, copy-fail central toast, blob create/revoke 1:1, /chat surface hard-asserted (playwright, hard gate)'
            return 0
            ;;
        run|all) ;;
        *) fail "usage: $0 [run|list]" ;;
    esac

    require_rpi
    web_require_browser

    local root evidence port url
    root="$(scenario_workspace "$SCENARIO")"
    evidence="$EVIDENCE_ROOT/$SCENARIO"
    printf '%s\n' "$TOKEN" >"$root/token"

    write_expected_payloads "$evidence"
    for f in expected-rich.md expected-rich-extra.md; do
        [ -s "$evidence/$f" ] || fail "$SCENARIO: missing extracted expectation $f"
    done

    port="$(web_start_mock "$root" "$evidence")"
    web_spawn_rpi "$root" "$evidence" "$port"
    url="$(web_wait_for_listener "$evidence")"
    log "$SCENARIO: listener at $url"

    local pw_status=0
    web_run_playwright "$url" "$evidence" "$root/pw" "$SCRIPT_DIR/reply_actions_test.mjs" \
        RPI_EXPECTED_RICH="$evidence/expected-rich.md" \
        RPI_EXPECTED_RICH_EXTRA="$evidence/expected-rich-extra.md" \
        "RPI_CHAT_URL=${url%/web}/chat" \
        "RPI_RUNNER_ORIGIN=${url%/web}" \
        "RPI_RUNNER_TOKEN=$TOKEN" \
        "RPI_RUNNER_CWD=$root/workspace" || pw_status=$?
    if [ "$pw_status" -eq 0 ]; then
        printf 'playwright: reply-actions vertical three-button stack + byte-exact clipboard/download + stable filename + touch + central toast-fail + /chat hard leg: PASSED\n' | tee "$evidence/playwright-summary.txt"
    elif [ "$pw_status" -eq 1 ]; then
        fail "$SCENARIO: playwright SETUP FAILED (node/chromium/npm unavailable)"
    else
        fail "$SCENARIO: playwright lane failed (exit $pw_status) — assertion/journey failure; never auto-concluded as feature-landed"
    fi

    web_sanity_http "$url"
    web_finish_lane "$SCENARIO"
}

main "$@"
