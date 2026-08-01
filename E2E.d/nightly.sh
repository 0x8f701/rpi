#!/usr/bin/env bash
set -euo pipefail
SCRIPT_DIR="$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
case "${1:-list}" in
    list|--list|--dry-run) "$SCRIPT_DIR/live/campaign.sh" list ;;
    run) "$SCRIPT_DIR/live/campaign.sh" run ;;
    *) printf 'usage: %s [run|list|--dry-run]\n' "$0" >&2; exit 2 ;;
esac
