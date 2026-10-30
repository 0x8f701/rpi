#!/usr/bin/env bash
# Full FEATURES.md inventory E2E.
#
# One command exercises shipped user-visible capabilities by running the
# existing release-profile suites in a fixed order (never two Web runners at
# once). It does not duplicate lane scripts; it orchestrates them.
#
# Usage:
#   bash E2E.d/features.sh           # run every stage
#   bash E2E.d/features.sh list        # feature → check map
#   bash E2E.d/features.sh crate      # crate --release only
#
# Requires: current-tree `target/release/rpi` or `target/release-dist/rpi`
# (or RPI_BIN pointing at one of those). Never launches target/debug.
set -euo pipefail
SCRIPT_DIR="$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
REPO_ROOT="$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd -P)"

unset RPI_WEB_DIST_READY WEB_SPAWN_TOKENLESS || true

resolve_rpi_bin() {
    local bin="${RPI_BIN:-}"
    if [ -z "$bin" ]; then
        if [ -x "$REPO_ROOT/target/release/rpi" ]; then
            bin="$REPO_ROOT/target/release/rpi"
        elif [ -x "$REPO_ROOT/target/release-dist/rpi" ]; then
            bin="$REPO_ROOT/target/release-dist/rpi"
        else
            printf 'features: missing release rpi (build cargo +1.88.0 build -p pi-cli --bin rpi --release)\n' >&2
            exit 1
        fi
    fi
    case "$bin" in
        /*) ;;
        *) bin="$REPO_ROOT/$bin" ;;
    esac
    case "$bin" in
        */target/debug/*)
            printf 'features: refusing debug-profile binary: %s\n' "$bin" >&2
            exit 1
            ;;
    esac
    [ -x "$bin" ] || {
        printf 'features: not executable: %s\n' "$bin" >&2
        exit 1
    }
    printf '%s\n' "$bin"
}

list_matrix() {
    cat <<'EOF'
FEATURES.md inventory → E2E checks (all --release / target/release rpi)

Surfaces
  Print / JSON / RPC     campaigns.sh run (version, faux-json, rpc-state)
  TUI                    campaigns.sh run + user_scenarios.sh + interactive_commands.sh
  Web listener           web/run.sh (50 lanes) + release/listen-web-smoke.sh
  ACP                    not a dedicated E2E lane (CLI `rpi agent`)

CLI / slash
  Builtins /help…/quit   interactive_commands.sh + rpc-state command catalog
  Plugins /run          web plugin_command + campaigns extension
  Processes /ps         web processes + user process_panel + rpc-state process-list

Agent loop
  Compact/rewind         user rewind_compact
  Steering queue         user steering_queue_handoff
  Settings watch         cargo -p pi-coding --lib --release settings_watch
  Retry chains           cargo -p pi-coding --lib --release retry_fallback
                         cargo -p pi-coding --test retry_fallback_chain --release

Sessions
  Multi-session          web sessions, switch_catalog_identity, session_move_delete
  Import                 web external_sessions
  Collab                 user_scenarios.sh (collab_scenario.sh)

Goals / todos / orchestration / workflows
  Goal / loop            web goal, loop_goal + user goal_lifecycle
  Todo child             web todo_child_session + user todo_child_session
  Subagents              web core + standalone web/subagents.sh + orchestration
  Workflows              web sessions_workflow + ci/workflow.sh release
  Personas               web personas
  Side chat              web extras + user btw_side_chat
  LAN Chat               web chat, chat_mermaid

Code review / files
  Code review            web code_review_* + user code_review_expand, code_review_submodule, code_review_operations
  File explorer           web commands_review, files_latency
  Mermaid                web mermaid_viewer, chat_mermaid, presentation

Safety / listen
  Trust / XSS            web xss, auth, auth_tokenless
  Pairing                web auth, appborder, sidebar_rail

Voice
  STT / realtime         web stt_rpc, realtime_rpc, realtime_webrtc

Extensibility
  Skills                 web skill_completion
  Extensions             campaigns extension + plugin_command

Installer
  install.sh            tests/install-sh-static.sh

Platforms
  Linux --release         this runner
  macOS/Windows native    .github/workflows/release.yml (not executed here)

Usage: bash E2E.d/features.sh
EOF
}

FEATURES_FAILED=0
FEATURES_REPORT=""

record_stage() {
    local name="$1" status="$2" extra="${3:-}"
    FEATURES_REPORT="${FEATURES_REPORT}${name}	${status}	${extra}
"
    printf '[features] %s %s%s\n' "$name" "$status" "${extra:+ ($extra)}"
    if [ "$status" != "PASS" ]; then
        FEATURES_FAILED=1
    fi
}

run_stage() {
    local name="$1"
    shift
    log "features: stage '$name' starting"
    local rc=0
    "$@" || rc=$?
    if [ "$rc" -eq 0 ]; then
        record_stage "$name" "PASS" ""
        log "features: stage '$name' PASSED"
        return 0
    fi
    record_stage "$name" "FAIL" "exit $rc"
    log "features: stage '$name' FAILED"
    return 0
}

prepare_web_bundle() {
    require_cmd npm
    require_cmd python3
    local web="$REPO_ROOT/crates/pi-cli/web"
    (cd "$web" && npx vite build)
    python3 "$SCRIPT_DIR/web/lib/web_dist_fingerprint.py" stamp "$web" "$web/dist"
    export RPI_WEB_DEV_DIR="$web/dist"
    unset RPI_WEB_DIST_READY || true
}

run_crate() {
    require_cmd cargo
    cargo +1.88.0 test -p pi-coding --lib --release settings_watch
    cargo +1.88.0 test -p pi-coding --lib --release retry_fallback
    cargo +1.88.0 test -p pi-coding --test retry_fallback_chain --release
}

run_installer() {
    [ -f "$REPO_ROOT/tests/install-sh-static.sh" ] || fail "features: missing tests/install-sh-static.sh"
    mkdir -p "$EVIDENCE_ROOT/installer-static"
    (cd "$REPO_ROOT" && sh tests/install-sh-static.sh) \
        >"$EVIDENCE_ROOT/installer-static/installer-static.log" 2>&1
}

require_tmux() {
    command -v tmux >/dev/null 2>&1 || fail "features: tmux is required for TUI feature coverage"
}

write_report() {
    local report="$EVIDENCE_ROOT/features/REPORT.md"
    mkdir -p "$EVIDENCE_ROOT/features"
    {
        printf '# FEATURES.md inventory E2E report\n\n'
        printf 'E2E_RUN_ID=%s\n' "$E2E_RUN_ID"
        printf 'RPI_BIN=%s\n\n' "$RPI_BIN"
        printf '| stage | result | note |\n|---|---|---|\n'
        printf '%s' "$FEATURES_REPORT" | awk -F '\t' '{printf "| %s | %s | %s |\n", $1, $2, $3}'
    } >"$report"
    printf 'features report: %s\n' "$report"
}

load_common() {
    export RPI_BIN
    E2E_RUN_ID="${E2E_RUN_ID:-$(date -u +%Y%m%dT%H%M%SZ)-features-$$}"
    export E2E_RUN_ID
    EVIDENCE_ROOT="${EVIDENCE_ROOT:-${TMPDIR:-/tmp}/rpi-e2e-evidence/$E2E_RUN_ID}"
    WORK_ROOT="${WORK_ROOT:-${TMPDIR:-/tmp}/rpi-e2e-work/$E2E_RUN_ID}"
    export EVIDENCE_ROOT WORK_ROOT
    # shellcheck source=lib/common.sh
    . "$SCRIPT_DIR/lib/common.sh"
}

run_all() {
    load_common
    prepare_roots
    require_rpi
    require_cmd python3
    require_cmd node
    require_tmux

    run_stage crate run_crate
    run_stage installer run_installer
    run_stage version bash "$SCRIPT_DIR/ci/campaigns.sh" version
    run_stage faux-json bash "$SCRIPT_DIR/ci/campaigns.sh" faux-json
    run_stage rpc-state bash "$SCRIPT_DIR/ci/campaigns.sh" rpc-state
    run_stage openai-schema bash "$SCRIPT_DIR/ci/campaigns.sh" openai-schema
    run_stage bash-tui bash "$SCRIPT_DIR/ci/campaigns.sh" bash-tui
    run_stage orchestration bash "$SCRIPT_DIR/ci/campaigns.sh" orchestration
    run_stage user bash "$SCRIPT_DIR/ci/campaigns.sh" user
    run_stage workflow bash "$SCRIPT_DIR/ci/workflow.sh" release
    run_stage extension bash "$SCRIPT_DIR/ci/campaigns.sh" extension
    run_stage interactive-commands bash "$SCRIPT_DIR/ci/interactive_commands.sh" run
    run_stage listen-web-smoke bash "$SCRIPT_DIR/release/listen-web-smoke.sh" run
    if prepare_web_bundle; then
        record_stage "web-bundle" "PASS" ""
        run_stage web bash "$SCRIPT_DIR/web/run.sh" run
        run_stage web-subagents bash "$SCRIPT_DIR/web/subagents.sh" run
    else
        record_stage "web-bundle" "FAIL" "vite/stamp"
        record_stage "web" "FAIL" "skipped: web-bundle"
        record_stage "web-subagents" "FAIL" "skipped: web-bundle"
    fi

    write_report
    if [ "$FEATURES_FAILED" -ne 0 ]; then
        fail "features: one or more stages FAILED (report: $EVIDENCE_ROOT/features/REPORT.md)"
    fi
    printf 'features passed\nevidence=%s\n' "$EVIDENCE_ROOT"
}

case "${1:-run}" in
    list|--list|--dry-run) list_matrix ;;
    crate)
        RPI_BIN="$(resolve_rpi_bin)"
        load_common
        prepare_roots
        run_crate
        printf 'features crate passed\n'
        ;;
    run|all)
        RPI_BIN="$(resolve_rpi_bin)"
        run_all
        ;;
    *)
        printf 'usage: %s [run|list|crate]\n' "$0" >&2
        exit 2
        ;;
esac
