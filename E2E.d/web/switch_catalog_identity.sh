#!/usr/bin/env bash
# Web negotiated catalog-identity switch E2E lane (playwright-only hard gate).
#
# Spawns the real `rpi --listen` binary with a token file and the loopback
# mock provider (--scenario sessions), seeds THREE representative large native
# sessions A/B/C (>=1500 recorded messages each, written directly as version-3
# JSONL fixtures — never 1500 browser prompts) plus one small Codex foreign
# source for imported/native alias restore, opens `/web` in a real
# browser, and asserts the frozen cold-vs-exact-ready switch contract end to
# end:
#   1. COLD (unloaded/stale/foreign target): exactly ONE correlated
#      switch_session response (subscribe-handoff-v1, source sessionId,
#      sessionPath, native catalog tuple, no incarnation field) + exactly one
#      subscribe (cursor 0) + one authoritative cold-full subscription cut
#      (exactly 1500 messages) — proven on ALL THREE cold legs: the bootstrap
#      restore of B (source derived from the boot authority primary), the
#      first user click B→A, and the never-loaded mobile C click.
#   2. EXACT-READY (same-socket repeated A→B→A): ZERO switch_session, ZERO
#      subscribe, no switching marker, no toast; the active row, transcript
#      ownership, and composer routing flip synchronously or by the next
#      animation frame; the per-authority session preference follows the
#      target; desktop AND mobile viewport.
#   3. the header renders NO #session-name element and no visible "session:"
#      label beside the model selector (desktop + mobile screenshot)
#
# Browser driver: playwright via npm (ephemeral install in the scenario work
# dir) with a system Chrome/Chromium binary or playwright's bundled chromium.
#
# Usage: bash E2E.d/web/switch_catalog_identity.sh [run|list]
set -euo pipefail
SCRIPT_DIR="$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
E2E_DIR="$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd -P)"
# shellcheck source=../lib/common.sh
. "$E2E_DIR/lib/common.sh"
# shellcheck source=lib/fixture.sh
. "$SCRIPT_DIR/lib/fixture.sh"

SCENARIO="web-switch-catalog-identity"
TOKEN="web-switch-catalog-identity-$$-$(date +%s)"

# ---------------------------------------------------------------------------
main() {
    case "${1:-run}" in
        list|--list|--dry-run)
            printf '%s\n' \
                'web-switch-catalog-identity - seeded 1500-message A/B/C fixtures: cold and exact-ready switches, desktop/mobile, transcript and composer identity (playwright, hard gate)'
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
    # Seed THREE representative large native sessions (A, B, C — 1500 recorded
    # messages each) under the isolated profile's native sessions tree,
    # `<agent>/sessions/--<encoded-cwd>--/<stamp>T000000-<id>.jsonl` with a
    # version-3 session header (cwd recorded) + session_info name + a chained
    # 1500-message user branch. The recorded message count is the whole point:
    # the cold authoritative cut must carry a genuinely large snapshot, and
    # the exact-ready fast path must restore it with zero wire frames. Seed
    # summary (COUNTS ONLY — never message content) lands in the evidence dir.
    # Session A additionally carries ONE oversized message (>8 KiB) for a real large-text fold.
    python3 - "$root/home/.rpi/sessions" "$root/workspace" "$evidence" <<'PY'
import datetime, json, os, pathlib, sys

sessions_root = pathlib.Path(sys.argv[1])
workspace = sys.argv[2]
evidence = pathlib.Path(sys.argv[3])

FIXTURE_MESSAGES = 1500


def encoded(cwd: str) -> str:
    s = cwd
    if s.startswith('/') or s.startswith('\\'):
        s = s[1:]
    return '--' + s.replace('/', '-').replace('\\', '-').replace(':', '-') + '--'


def write_seed(sid: str, name: str, stamp: str, large_at: int = 0) -> pathlib.Path:
    directory = sessions_root / encoded(workspace)
    directory.mkdir(parents=True, exist_ok=True)
    lines = [
        {'type': 'session', 'version': 3, 'id': sid, 'timestamp': stamp, 'cwd': workspace},
        {'type': 'session_info', 'id': 'si-1', 'parentId': None, 'timestamp': stamp, 'name': name},
    ]
    parent = None
    for index in range(1, FIXTURE_MESSAGES + 1):
        last = index == FIXTURE_MESSAGES
        text = f'{sid}-tail' if last else f'{sid}-msg-{index:04d}'
        if index == large_at:
            text = f'{text} ' + ('fold-retention ' * 1200)
        lines.append({
            'type': 'message', 'id': f'm{index:04d}',
            'parentId': None if index == 1 else f'm{index - 1:04d}',
            'timestamp': stamp,
            'message': {'role': 'user', 'content': [{'type': 'text', 'text': text}], 'timestamp': index},
        })
    target = directory / f'{stamp[:10]}T000000-{sid}.jsonl'
    target.write_text(''.join(json.dumps(r, separators=(',', ':')) + '\n' for r in lines), encoding='utf-8')
    return target


stamp = datetime.datetime.now(datetime.timezone.utc).isoformat().replace('+00:00', 'Z')
file_c = write_seed('switch-catalog-c', 'switch-catalog-c', stamp)
file_b = write_seed('switch-catalog-b', 'switch-catalog-b', stamp)
file_a = write_seed('switch-catalog-a', 'switch-catalog-a', stamp, large_at=1400)
# Pin mtimes deterministically: A at T, B at T+1s. B is the NEWEST catalog
# row, so seeding the page-authority preference to B exercises the exact
# frozen boot contract: the bootstrap restore of B is a COLD negotiated
# switch (one correlated switch_session + subscribe + full cut), and the
# first USER click A is the frozen "first cold B→A" leg.
base = file_a.stat().st_mtime
os.utime(file_c, (base, base))
os.utime(file_a, (base + 1, base + 1))
os.utime(file_b, (base + 2, base + 2))
(evidence / 'seed-summary.json').write_text(json.dumps({
    'sessionsRoot': str(sessions_root),
    'encodedCwdDir': encoded(workspace),
    'sessionIds': ['switch-catalog-a', 'switch-catalog-b', 'switch-catalog-c'],
    'codexSessionId': 'codex-switch-catalog',
    'recordedMessagesPerSession': FIXTURE_MESSAGES,
    'largeFoldMessageIndex': 1400,
}, indent=2), encoding='utf-8')

# Foreign Codex source for imported/native alias restore after reconnect.
# Small rollout (not 1500 browser prompts): clicking it imports a native copy
# whose runtime id is the reconnect identity, never the foreign catalog id.
home = sessions_root.parent.parent
codex_dir = home / '.codex' / 'sessions'
codex_dir.mkdir(parents=True, exist_ok=True)
codex_id = 'codex-switch-catalog'
(codex_dir / f'rollout-{codex_id}.jsonl').write_text(
    json.dumps({'type': 'session_meta', 'payload': {'id': codex_id, 'cwd': workspace}}, separators=(',', ':')) + '\n'
    + json.dumps({'type': 'response_item', 'payload': {'type': 'message', 'role': 'user', 'content': [{'type': 'input_text', 'text': 'codex-switch-catalog-prompt'}]}}, separators=(',', ':')) + '\n'
    + json.dumps({'type': 'response_item', 'payload': {'type': 'message', 'role': 'assistant', 'content': [{'type': 'output_text', 'text': 'codex-switch-catalog-reply'}]}}, separators=(',', ':')) + '\n',
    encoding='utf-8',
)
PY

    MOCK_SCENARIO=sessions port="$(web_start_mock "$root" "$evidence")"
    web_spawn_rpi "$root" "$evidence" "$port"
    url="$(web_wait_for_listener "$evidence")"
    log "$SCENARIO: listener at $url"

    local pw_status=0
    web_run_playwright "$url" "$evidence" "$root/playwright" "$SCRIPT_DIR/switch_catalog_identity_test.mjs" \
        || pw_status=$?
    if [ "$pw_status" -eq 0 ]; then
        printf 'playwright: catalog-identity cold-vs-exact-ready switch journey (A/B/C, mobile cold C, retained identity, demand-window exactness) + header label removal: PASSED\n' | tee "$evidence/playwright-summary.txt"
    elif [ "$pw_status" -eq 1 ]; then
        fail "$SCENARIO: playwright SETUP FAILED (node/chromium/npm unavailable)"
    else
        fail "$SCENARIO: playwright lane failed (exit $pw_status)"
    fi

    web_sanity_http "$url"
    web_finish_lane "$SCENARIO"
}

main "$@"