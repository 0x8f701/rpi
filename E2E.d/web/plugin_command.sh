#!/usr/bin/env bash
# Web trusted-plugin command regression (playwright-only hard gate).
#
# Installs a QuickJS directory plugin into the isolated RPI_HOME via
# `rpi plugin install`, then opens /web and asserts the Command picker lists
# /e2e-plugin-ping, selecting it inserts the draft without auto-submit, and
# Enter dispatches RPC run so a summary bubble shows pong. Desktop and mobile
# screenshots. Missing node/chromium FAILS (exit 1).
#
# Usage: bash E2E.d/web/plugin_command.sh [run|list]
set -euo pipefail
SCRIPT_DIR="$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
E2E_DIR="$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd -P)"
# shellcheck source=../lib/common.sh
. "$E2E_DIR/lib/common.sh"
# shellcheck source=lib/fixture.sh
. "$SCRIPT_DIR/lib/fixture.sh"

SCENARIO="web-plugin-command"
TOKEN="web-plugin-command-e2e-token-$$-$(date +%s)"
PLUGIN_ID="e2e-plugin-command"
PLUGIN_COMMAND="e2e-plugin-ping"
PLUGIN_RESULT="pong"

seed_plugin() {
    local root="$1"
    mkdir -p "$root/plugin"
    cat >"$root/plugin/pi-extension.json" <<EOF
{"schemaVersion":1,"id":"$PLUGIN_ID","version":"0.1.0","runtime":"quickjs","entry":"index.mjs","capabilities":["commands"]}
EOF
    cat >"$root/plugin/index.mjs" <<EOF
export default function (pi) {
  pi.registerCommand("$PLUGIN_COMMAND", {
    description: "E2E plugin ping",
    handler: async () => "$PLUGIN_RESULT",
  });
}
EOF
}

install_plugin() {
    local root="$1"
    mkdir -p "$root/home/.rpi"
    env -i \
        HOME="$root/home" USERPROFILE="$root/home" \
        PATH="${PATH:-/usr/bin:/bin}" LANG="${LANG:-C.UTF-8}" LC_ALL="${LC_ALL:-C.UTF-8}" \
        RPI_HOME="$root/home/.rpi" \
        RPI_OFFLINE=1 RPI_SKIP_VERSION_CHECK=1 \
        "$RPI_BIN" --offline plugin install "$root/plugin" \
        >"$root/plugin-install.stdout" 2>"$root/plugin-install.stderr"
    grep -q 'trusted and loadable' "$root/plugin-install.stdout" \
        || fail "$SCENARIO: plugin install did not report trusted and loadable"
    mkdir -p "$EVIDENCE_ROOT/$SCENARIO"
    cp "$root/plugin-install.stdout" "$EVIDENCE_ROOT/$SCENARIO/plugin-install.stdout"
    cp "$root/plugin-install.stderr" "$EVIDENCE_ROOT/$SCENARIO/plugin-install.stderr"
}

main() {
    case "${1:-run}" in
        list|--list|--dry-run)
            printf '%s\n' \
                'web-plugin-command - trusted marketplace plugin command in the Web picker; select inserts /e2e-plugin-ping (no auto-submit); Enter dispatches RPC run and paints pong (PLAYWRIGHT-ONLY, hard-fail)'
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
    mkdir -p "$root/workspace"
    printf '%s\n' "$TOKEN" >"$root/token"
    seed_plugin "$root"
    port="$(web_start_mock "$root" "$evidence")"
    install_plugin "$root"
    web_spawn_rpi "$root" "$evidence" "$port"
    url="$(web_wait_for_listener "$evidence")"
    log "$SCENARIO: listener at $url"

    local pw_status=0
    web_run_playwright "$url" "$evidence" "$root/playwright" \
        "$SCRIPT_DIR/plugin_command_test.mjs" \
        "RPI_PLUGIN_COMMAND=$PLUGIN_COMMAND" \
        "RPI_PLUGIN_RESULT=$PLUGIN_RESULT" || pw_status=$?
    if [ "$pw_status" -eq 0 ]; then
        printf 'playwright: trusted plugin command picker + run pong PASSED\n' | tee "$evidence/playwright-summary.txt"
    elif [ "$pw_status" -eq 1 ]; then
        fail "$SCENARIO: playwright SETUP FAILED (node/chromium/npm unavailable)"
    else
        fail "$SCENARIO: playwright lane failed (exit $pw_status)"
    fi
    web_sanity_http "$url"
    web_finish_lane "$SCENARIO"
}

main "$@"
