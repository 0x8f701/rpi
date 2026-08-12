#!/usr/bin/env bash
set -euo pipefail
SCRIPT_DIR="$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
REPO_ROOT="$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd -P)"
case "${1:-run}" in
    list|--list|--dry-run)
        "$SCRIPT_DIR/ci/campaigns.sh" list
        printf '%s\n' 'installer.static - checked-in install.sh regression suite'
        printf '%s\n' 'web - unified client suite: core (load/auth/stream/abort/todo/rich/workflow/settings/session/subagents), goal, xss, abort, reconnect, mobile, auth, extras, sessions, session_restore, projects, commands_review, attachments (E2E_CI_WEB=1)'
        ;;
    run)
        [ -f "$REPO_ROOT/tests/install-sh-static.sh" ] || {
            printf 'ci.sh: required installer regression script is missing: tests/install-sh-static.sh\n' >&2
            exit 1
        }
        "$SCRIPT_DIR/ci/campaigns.sh" run
        mkdir -p "${EVIDENCE_ROOT:-${TMPDIR:-/tmp}/rpi-e2e-evidence/installer-static}"
        (cd "$REPO_ROOT" && sh tests/install-sh-static.sh) \
            > "${EVIDENCE_ROOT:-${TMPDIR:-/tmp}/rpi-e2e-evidence/installer-static}/installer-static.log" 2>&1
        if [ "${E2E_CI_TMUX:-0}" = 1 ]; then
            "$SCRIPT_DIR/ci/campaigns.sh" tmux-matrix
        fi
        if [ "${E2E_CI_EXTENSION:-0}" = 1 ]; then
            "$SCRIPT_DIR/ci/campaigns.sh" extension
        fi
        if [ "${E2E_CI_WEB:-0}" = 1 ]; then
            # The unified runner aggregates every web lane (core, goal, xss,
            # abort, reconnect, switch, mobile, auth, extras, sessions).
            "$SCRIPT_DIR/web/run.sh" run
        fi
        ;;
    *)
        printf 'usage: %s [run|list|--dry-run]\n' "$0" >&2
        exit 2
        ;;
esac
