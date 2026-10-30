#!/usr/bin/env bash
# Web runtime release matrix lane — PLAYWRIGHT-ONLY hard gate (runtime_matrix_test.mjs):
# fill to MAX_LOADED_RUNTIMES, exact LRU eviction while page A is offline, cold reopen, busy/streaming/process/Goal/Todo protection, zero-wire sub-second switches, bounded RSS/fd/socket.
# Needs MOCK_SECOND_MODEL=1 + MOCK_REASONING=1. Usage: bash E2E.d/web/runtime_matrix.sh [run|list]
set -euo pipefail
SCRIPT_DIR="$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
E2E_DIR="$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd -P)"
# shellcheck source=../lib/common.sh
. "$E2E_DIR/lib/common.sh"
# shellcheck source=lib/fixture.sh
. "$SCRIPT_DIR/lib/fixture.sh"

SCENARIO="web-runtime-matrix"
TOKEN="web-runtime-matrix-e2e-token-$$-$(date +%s)"

main() {
    case "${1:-run}" in
        list|--list|--dry-run)
            printf '%s\n' \
                'web-runtime-matrix - eviction/resume release matrix: current loaded-runtime cap, missed LRU unload while the browser is offline, protected idle LRU retirement, no manual close, cold reopen restores exact messages/cursor/model/thinking/Todo/Goal under a new incarnation, exact-ready zero-wire sub-second switches, bounded before/after RSS/fd/socket (PLAYWRIGHT-ONLY, hard-fail)'
            return 0
            ;;
        run|all) ;;
        *) fail "usage: $0 [run|list]" ;;
    esac

    require_rpi
    require_cmd node

    local root evidence port url pw_status=0
    root="$(scenario_workspace "$SCENARIO")"
    git -C "$root/workspace" init -q
    git -C "$root/workspace" config user.email "web-runtime-matrix@example.com"
    git -C "$root/workspace" config user.name "Web Runtime Matrix"
    git -C "$root/workspace" config commit.gpgsign false
    printf 'web runtime matrix seed\n' >"$root/workspace/seed.txt"
    git -C "$root/workspace" add -- seed.txt
    git -C "$root/workspace" commit -q -m "web runtime matrix seed"
    evidence="$EVIDENCE_ROOT/$SCENARIO"
    printf '%s\n' "$TOKEN" >"$root/token"
    cat >"$root/home/.rpi/settings.json" <<'EOF'
{
  "orchestration": {
    "tasks": true,
    "maxConcurrency": 2,
    "maxRecursionDepth": 2,
    "isolation": "none"
  }
}
EOF

    port="$(MOCK_SCENARIO=sessions MOCK_SECOND_MODEL=1 MOCK_REASONING=1 web_start_mock "$root" "$evidence")"
    web_spawn_rpi "$root" "$evidence" "$port"
    url="$(web_wait_for_listener "$evidence")"
    log "$SCENARIO: listener at $url"
    mkdir -p "$root/playwright/lib"
    cp "$SCRIPT_DIR/lib/p0_resource_driver.mjs" "$root/playwright/lib/p0_resource_driver.mjs"
    cp "$SCRIPT_DIR/lib/p0_resource_metrics.mjs" "$root/playwright/lib/p0_resource_metrics.mjs"
    cp "$SCRIPT_DIR/lib/p0_native_session.mjs" "$root/playwright/lib/p0_native_session.mjs"

    web_run_playwright "$url" "$evidence" "$root/playwright" "$SCRIPT_DIR/runtime_matrix_test.mjs" \
        RPI_MOCK_PORT="$port" RPI_PID="${RPI_PID:-}" || pw_status=$?
    if [ "$pw_status" -ne 0 ]; then
        fail "$SCENARIO: playwright lane failed (exit $pw_status) — runtime-matrix lane is playwright-only"
    fi

    web_sanity_http "$url"
    web_finish_lane "$SCENARIO"
}

main "$@"