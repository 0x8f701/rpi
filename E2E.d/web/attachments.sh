#!/usr/bin/env bash
# Web composer attachment intake E2E lane (playwright-only hard gate).
#
# Spawns the real `rpi --listen` binary with a token file and the loopback
# mock provider (--scenario steering; no prompts are sent, so the mock only
# serves the model catalog), opens `/web` in a real browser, and asserts the
# v0.2.10 Web composer attachment intake — clipboard image paste, multi-file
# picker, multi-file drag/drop, Rust/TypeScript code upload, and the outgoing
# RPC prompt frame content/order:
#   - Paste image via real ClipboardEvent/DataTransfer with a valid tiny PNG
#     File — dispatch is canceled (defaultPrevented) only for files; a
#     text-only ClipboardEvent is NOT canceled.
#   - Picker sets 2 code files together (.rs + .ts) via the hidden
#     input[type=file] — 2 code chips appear with RS/TS badge labels.
#   - Drop sends 2 code files together via synthetic DragEvent on the footer —
#     drop-active highlight (footer[data-drop-active], .composer-drop) on
#     dragenter, clears on drop; 2 code chips appear.
#   - Chips preserve global intake order: pasted image -> picker .rs ->
#     picker .ts -> drop .rs -> drop .ts.
#   - PDF picker yields a visible error toast (toast--error with the PDF
#     name) and NO chip (PDFs are unsupported, rejected at intake).
#   - Send dispatches a prompt RPC — observed on the outgoing WS frame
#     (deterministic; the provider round-trip is NOT required): one images
#     block (the pasted PNG), message contains filename+source for ALL code
#     files in the same order as the chips, and NO PDF content.
#   - Attachments clear after dispatch (#composer-attachments leaves DOM).
#   - The sent user bubble renders the image thumbnail INLINE with the typed
#     text (never the old "(image attached)" placeholder).
#   - A second image-only multi-image send renders 2 distinct thumbnails with
#     no text; ACK reconcile never duplicates/flickers bubbles.
#   - Mobile viewport: the multi-image grid causes no horizontal overflow.
#   - Reload restores BOTH user bubbles with their thumbnails from history
#     (persisted prompt image ContentBlocks render directly).
#
# Browser driver: playwright via npm (ephemeral install in the scenario work
# dir) with a system Chrome/Chromium binary or playwright's bundled chromium.
# Missing node, a failed playwright install, or no usable Chromium FAILS the
# lane (exit 1, setup); assertion failures exit 2+.
#
# Usage: bash E2E.d/web/attachments.sh [run|list]
set -euo pipefail
SCRIPT_DIR="$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
E2E_DIR="$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd -P)"
# shellcheck source=../lib/common.sh
. "$E2E_DIR/lib/common.sh"
# shellcheck source=lib/fixture.sh
. "$SCRIPT_DIR/lib/fixture.sh"

SCENARIO="web-attachments"
TOKEN="web-attachments-e2e-token-$$-$(date +%s)"

# ---------------------------------------------------------------------------
main() {
    case "${1:-run}" in
        list|--list|--dry-run)
            printf '%s\n' \
                'web-attachments - clipboard image paste (canceled for files, not for text), multi-file picker (.rs+.ts), multi-file drag/drop with drop-active highlight, chip global intake order, PDF rejection toast + no chip, outgoing prompt WS frame: 1 image block + code files in order + no PDF, attachments clear after dispatch (playwright, hard gate)'
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

    port="$(web_start_mock "$root" "$evidence")"
    web_spawn_rpi "$root" "$evidence" "$port"
    url="$(web_wait_for_listener "$evidence")"
    log "$SCENARIO: listener at $url"

    local pw_status=0
    web_run_playwright "$url" "$evidence" "$root/playwright" \
        "$SCRIPT_DIR/attachments_test.mjs" || pw_status=$?
    if [ "$pw_status" -eq 0 ]; then
        printf 'playwright: attachment paste/picker/drop + outgoing prompt frame: PASSED\n' | tee "$evidence/playwright-summary.txt"
    elif [ "$pw_status" -eq 1 ]; then
        fail "$SCENARIO: playwright SETUP FAILED (node/chromium/npm unavailable)"
    else
        fail "$SCENARIO: playwright lane failed (exit $pw_status)"
    fi

    web_sanity_http "$url"
    web_finish_lane "$SCENARIO"
}

main "$@"