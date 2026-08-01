#!/usr/bin/env bash
set -euo pipefail
SCRIPT_DIR="$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
case "${1:-list}" in
    list|--list|--dry-run) "$SCRIPT_DIR/live/campaign.sh" list ;;
    architecture-research|zig-build-fix|subagent-steering|compact-overflow) "$SCRIPT_DIR/live/campaign.sh" "$1" ;;
    run) "$SCRIPT_DIR/live/campaign.sh" run ;;
    *) printf 'usage: %s [list|--dry-run|run|architecture-research|zig-build-fix|subagent-steering|compact-overflow]\n' "$0" >&2; exit 2 ;;
esac
