#!/usr/bin/env bash
# Web XSS + secret-redaction E2E lane (playwright-only hard gate).
#
# Spawns the real `rpi --listen` binary with a token file and the loopback
# mock provider in its dedicated `xss` scenario (E2E.d/lib/user_mock_server.py
#   --scenario xss: every request streams raw HTML that must never execute plus
#   a credential-shaped token), opens `/web` in a real browser, and asserts:
#   1. the hostile payload renders as INERT TEXT (no dialog, no window.__xss
#      global, no <img>/<script> element inside the assistant message, raw
#      payload visible escaped) — regression guard for redact.ts + markdown.ts
#   2. the credential is redacted to [REDACTED] in every view — the raw
#      secret never appears anywhere in the page
#   3. a browser-realistic extension_ui_request approval card (fixture QuickJS
#      extension's input hook issues an interactive confirm): the card renders
#      the hostile title/message as inert text, no toast carries the payload,
#      no error toast appears, and embedded credentials are redacted
#
# Browser driver: playwright via npm (ephemeral install in the scenario work
# dir) with a system Chrome/Chromium binary or playwright's bundled chromium.
# Missing node, a failed playwright install, or no usable Chromium FAILS the
# lane (exit 1, setup); assertion failures exit 2+.
#
# Usage: bash E2E.d/web/xss.sh [run|list]
set -euo pipefail
SCRIPT_DIR="$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
E2E_DIR="$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd -P)"
# shellcheck source=../lib/common.sh
. "$E2E_DIR/lib/common.sh"
# shellcheck source=lib/fixture.sh
. "$SCRIPT_DIR/lib/fixture.sh"

SCENARIO="web-xss"
MOCK_SCENARIO="xss"
TOKEN="web-xss-e2e-token-$$-$(date +%s)"
SK=$(printf '%s%s%s' s k -)
XSS_TEXT="unsafe <img src=x onerror=alert(1)><script>window.__xss='pwned'</script> and the leaked credential ${SK}test-secret-""abcdef0123456789."
XSS_SECRET="${SK}test-secret-""abcdef0123456789"
# Browser-realistic extension_ui_request approval card: a trusted QuickJS
# fixture extension's input hook issues an interactive confirm when the
# prompt text carries the marker; the rpi listener relays it to the web
# client as extension_ui_request. The hostile title/message must render as
# INERT TEXT on the approval card (never execute) and never leak into a
# toast; the embedded credential must be redacted like every other view.
APPROVAL_MARKER="REQUEST_APPROVAL"
APPROVAL_EXT_ID="web-xss-approval"
APPROVAL_SECRET="${SK}approval-""secret-abcdef0123456789"

# ---------------------------------------------------------------------------
main() {
    case "${1:-run}" in
        list|--list|--dry-run)
            printf '%s\n' \
                'web-xss - hostile model output renders as inert text (no dialog/global/elements) + a credential-shaped token redacted to [REDACTED] (playwright, hard gate)'
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

    # Approval fixture extension (loaded via --extension by this lane only).
    mkdir -p "$root/ext"
    cat >"$root/ext/pi-extension.json" <<EOF
{"schemaVersion":1,"id":"$APPROVAL_EXT_ID","runtime":"quickjs","entry":"index.mjs","capabilities":["ui","event_hooks"],"uiCapabilities":["confirm"]}
EOF
    cat >"$root/ext/index.mjs" <<EOF
export default function (pi) {
  pi.on("input", async (event, ctx) => {
    const text = event && typeof event.text === "string" ? event.text : "";
    if (!text.includes("$APPROVAL_MARKER")) return;
    try {
      await ctx.ui.confirm(
        "Approve <img src=x onerror=window.__xss2='pwned'>?",
        "proceed <script>window.__xss2='pwned'</script> with $APPROVAL_SECRET",
        { timeout: 2000 }
      );
    } catch (e) {
      // The web client renders "answer in the terminal" and never answers;
      // the 2s timeout lets the turn settle so the prompt round-trips.
    }
  });
}
EOF

    port="$(web_start_mock "$root" "$evidence")"
    WEB_SPAWN_EXTENSION="$root/ext" web_spawn_rpi "$root" "$evidence" "$port"
    url="$(web_wait_for_listener "$evidence")"
    log "$SCENARIO: listener at $url"

    local pw_status=0
    web_run_playwright "$url" "$evidence" "$root/playwright" "$SCRIPT_DIR/xss_test.mjs" \
        RPI_XSS_TEXT="$XSS_TEXT" RPI_XSS_SECRET="$XSS_SECRET" \
        RPI_APPROVAL_MARKER="$APPROVAL_MARKER" RPI_APPROVAL_EXT_ID="$APPROVAL_EXT_ID" \
        RPI_APPROVAL_SECRET="$APPROVAL_SECRET" || pw_status=$?
    if [ "$pw_status" -eq 0 ]; then
        printf 'playwright: XSS inert render + [REDACTED] + approval card safe text/no toast: PASSED\n' | tee "$evidence/playwright-summary.txt"
    elif [ "$pw_status" -eq 1 ]; then
        fail "$SCENARIO: playwright SETUP FAILED (node/chromium/npm unavailable)"
    else
        fail "$SCENARIO: playwright lane failed (exit $pw_status)"
    fi

    web_sanity_http "$url"
    web_finish_lane "$SCENARIO"
}

main "$@"
