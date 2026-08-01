#!/usr/bin/env bash
set -euo pipefail
SCRIPT_DIR="$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
. "$SCRIPT_DIR/../lib/common.sh"
list_scenarios() {
    printf '%s\n' \
        'campaign.process - supervised process spawn/list through RPC' \
        'campaign.loop - create/list a scheduled loop through RPC' \
        'campaign.goal - create/get a goal through RPC' \
        'campaign.todo - dependency-blocked todo projection through RPC' \
        'campaign.tools - command catalog plus deterministic foreground bash tool' \
        'campaign.session - name/state/tree session lifecycle through RPC'
}
case "${1:-list}" in list|--list|--dry-run) list_scenarios; exit 0 ;; process|loop|goal|todo|tools|session) scenario="$1" ;; run) scenario=all ;; *) fail "usage: $0 [list|--dry-run|run|process|loop|goal|todo|tools|session]" ;; esac
prepare_roots; require_rpi; require_cmd python3
root="$(scenario_workspace "campaign-$scenario")"; evidence="$EVIDENCE_ROOT/campaign-$scenario"
run_with_timeout 40 python3 "$E2E_DIR/lib/run_rpc_campaign.py" --scenario "$scenario" --rpi "$RPI_BIN" --home "$root/home" --workspace "$root/workspace" --output "$evidence/output.jsonl" --stderr "$evidence/stderr.log"
python3 - "$evidence/output.jsonl" <<'PY'
import json, sys
rows=[json.loads(x) for x in open(sys.argv[1]) if x.strip()]
responses=[x for x in rows if x.get('type')=='response']
assert responses and all(x.get('success') is True for x in responses), rows
PY
printf '%s campaign passed\nevidence=%s\n' "$scenario" "$evidence"
