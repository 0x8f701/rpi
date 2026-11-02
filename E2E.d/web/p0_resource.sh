#!/usr/bin/env bash
# Standalone P0 session-acceptance harness (NOT in run.sh LANES).
#
# Own files only: does not edit sessions_test.mjs, sessions.sh,
# user_mock_server.py, production source, or lib/fixture.sh. Uses a
# self-contained local mock (lib/p0_resource_mock.py) because the shared
# sessions scenario cannot be extended from this slice.
#
# Live contract (long run, not part of the unit check):
#   1. generate/load an exact 20 MiB native v3 JSONL session
#   2. fill the 8-runtime pool, idle-LRU evict, reopen the retired row
#   3. measure action -> authoritative interactive (subscribe cut) latency
#   4. two browser clients on one listener + background session streaming
#   5. process-tree RSS plateau, connection-owned pressure install
#      (paused unread subscribe; fd signal or listener-local-port
#      established-count fallback), causal destroy, gated release
#   6. record unexpected WS close codes including 1006
#   7. generate/load an exact 2 MiB selected-branch pressure session
#      (connection-pressure prerequisite; passed to the browser test)
#
# Usage:
#   node E2E.d/web/lib/p0_resource_unit.mjs          # syntax/unit checks
#   bash E2E.d/web/lib/p0_resource_shell_unit.sh     # shell unit (mock lifecycle, no rpi)
#   bash E2E.d/web/p0_resource.sh [run|list]         # live (needs rpi)

set -euo pipefail
SCRIPT_DIR="$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
E2E_DIR="$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd -P)"
# shellcheck source=../lib/common.sh
. "$E2E_DIR/lib/common.sh"
# shellcheck source=lib/fixture.sh
. "$SCRIPT_DIR/lib/fixture.sh"

SCENARIO="web-p0-resource"
TOKEN="web-p0-resource-e2e-token-$$-$(date +%s)"
NATIVE_ID="p0-native-20mib"
PRESSURE_ID="p0-pressure-2mib"

start_local_mock() {
    # Runs in the PARENT shell (never via command substitution): the
    # register_pid E2E_PIDS update must reach the EXIT trap so cleanup owns
    # the mock PID on every success/failure path. The port is returned
    # through the global MOCK_PORT; mock.pid is also written for
    # file-based consumers (same convention as web_start_mock).
    local root="$1" evidence="$2" deadline
    # NOTE: port_file MUST be declared after root — under `set -u` bash 5.2
    # evaluates `$root` in the same-statement initializer before the new
    # local takes effect (see web_start_mock in lib/fixture.sh).
    local port_file="$root/mock-port.txt"
    python3 "$SCRIPT_DIR/lib/p0_resource_mock.py" --port-file "$port_file" \
        >"$evidence/mock-server.log" 2>&1 &
    printf '%s\n' "$!" >"$root/mock.pid"
    register_pid "$!"
    deadline=$((SECONDS + 15))
    while [ ! -s "$port_file" ] && [ "$SECONDS" -lt "$deadline" ]; do sleep 0.2; done
    [ -s "$port_file" ] || fail "$SCENARIO: local mock did not write its port file"
    MOCK_PORT="$(cat "$port_file")"
    mkdir -p "$root/home/.rpi"
    cat >"$root/home/.rpi/models.json" <<EOF
{
  "providers": {
    "user-steering": {
      "baseUrl": "http://127.0.0.1:$MOCK_PORT",
      "api": "openai-completions",
      "models": [
        { "id": "mock", "name": "P0 Resource Mock", "contextWindow": 32768, "maxTokens": 2048 }
      ]
    }
  }
}
EOF
}

copy_p0_libs() {
    local work="$1"
    mkdir -p "$work/lib"
    cp "$SCRIPT_DIR/lib/p0_resource_metrics.mjs" "$work/lib/p0_resource_metrics.mjs"
    cp "$SCRIPT_DIR/lib/p0_native_session.mjs" "$work/lib/p0_native_session.mjs"
    cp "$SCRIPT_DIR/lib/p0_resource_driver.mjs" "$work/lib/p0_resource_driver.mjs"
}

main() {
    case "${1:-run}" in
        list|--list|--dry-run)
            printf '%s\n' \
                'web-p0-resource - standalone 20 MiB native session + 8-runtime pool/evict/reopen + authoritative interactive latency + dual browser clients + connection-pressure RSS plateau/install/release (fd or listener-port signal) + WS 1006 evidence (PLAYWRIGHT, not in LANES)'
            return 0
            ;;
        run|all) ;;
        *) fail "usage: $0 [run|list]" ;;
    esac

    require_rpi
    require_cmd node
    require_cmd python3

    local root evidence url native_path pressure_path
    root="$(scenario_workspace "$SCENARIO")"
    evidence="$EVIDENCE_ROOT/$SCENARIO"
    printf '%s\n' "$TOKEN" >"$root/token"

    native_path="$root/home/.rpi/sessions/--$(printf '%s' "$root/workspace" | sed 's#^/##; s#[/\\:]#-#g')--/2026-08-31T000000-${NATIVE_ID}.jsonl"
    node --input-type=module -e "
      import { writeExactNativeSession } from 'file://$SCRIPT_DIR/lib/p0_native_session.mjs';
      await writeExactNativeSession({
        filePath: process.argv[1],
        cwd: process.argv[2],
        sessionId: process.argv[3],
      });
    " "$native_path" "$root/workspace" "$NATIVE_ID"

    pressure_path="$root/home/.rpi/sessions/--$(printf '%s' "$root/workspace" | sed 's#^/##; s#[/\\:]#-#g')--/2026-08-31T000000-${PRESSURE_ID}.jsonl"
    node --input-type=module -e "
      import { writeExactSelectedPressureSession } from 'file://$SCRIPT_DIR/lib/p0_native_session.mjs';
      await writeExactSelectedPressureSession({
        filePath: process.argv[1],
        cwd: process.argv[2],
        sessionId: process.argv[3],
      });
    " "$pressure_path" "$root/workspace" "$PRESSURE_ID"

    start_local_mock "$root" "$evidence"
    web_spawn_rpi "$root" "$evidence" "$MOCK_PORT"
    url="$(web_wait_for_listener "$evidence")"
    log "$SCENARIO: listener at $url pid=$RPI_PID"

    local pw_status=0
    mkdir -p "$root/playwright/lib"
    copy_p0_libs "$root/playwright"
    web_run_playwright "$url" "$evidence" "$root/playwright" "$SCRIPT_DIR/p0_resource_test.mjs" \
        RPI_PID="${RPI_PID:-}" \
        RPI_NATIVE_SESSION_PATH="$native_path" \
        RPI_NATIVE_CWD="$root/workspace" \
        RPI_NATIVE_SESSION_ID="$NATIVE_ID" \
        RPI_PRESSURE_SESSION_PATH="$pressure_path" \
        RPI_PRESSURE_SESSION_ID="$PRESSURE_ID" \
        || pw_status=$?
    if [ "$pw_status" -eq 0 ]; then
        printf 'playwright: p0 session acceptance harness: PASSED\n' | tee "$evidence/playwright-summary.txt"
    elif [ "$pw_status" -eq 1 ]; then
        fail "$SCENARIO: playwright SETUP FAILED (node/chromium/npm unavailable)"
    else
        fail "$SCENARIO: playwright lane failed (exit $pw_status)"
    fi

    web_sanity_http "$url"
    web_finish_lane "$SCENARIO"
}

# Invoke main only when executed directly; sourcing this file (the shell
# unit) defines the functions and globals without starting the harness.
if [ "${BASH_SOURCE[0]}" = "$0" ]; then
    main "$@"
fi
