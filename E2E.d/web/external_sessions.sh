#!/usr/bin/env bash
# Web external-session discovery / immutable import / selected-row export E2E
# lane — PLAYWRIGHT-ONLY (no agent-browser fallback, no skip).
#
# Seeds deterministic Pi, OMP, Codex, and Grok sessions plus an
# `agentSession` (Slock `.slock/agents/<id>` cwd) and a `bootstrapOnly`
# native row under the isolated fixture HOME layout that
# SessionCatalog::from_env discovers, starts the real `rpi --listen` from a
# temp workspace, opens `/web` in a real Playwright browser, and asserts the
# named behavior matrix in external_sessions_test.mjs:
#   - boot + token connect
#   - Web-only default discovery/provider grouping (rpi/Pi/OMP/Codex/Grok)
#   - Agent sessions subgroup default-collapsed; bootstrapOnly row hidden by
#     default; search reveals it; after switch and search-clear the subgroup
#     collapses again and expanding it shows the still-active row
#   - foreign Pi/Codex/OMP selection imports immutable rpi native copies
#   - native and Pi sidebar Export HTML download the selected row without
#     activation; failure produces one toast and no download
#   - explicit sessionImportSources: [] is native-only (foreign rows gone)
#   - zero page/console errors and screenshot evidence
#
# Evidence publication is failure-clean and set-atomic as ONE lane-set
# commit: the shell generates ONE invocation nonce and passes it to BOTH
# phase drivers; each phase stages its artifacts (screenshots, coverage-
# assertions-<phase>.json, import-result.json) into the ONE shared staging
# dir derived from that nonce, registers an artifact only after its
# write/screenshot succeeded, and on its own success commits ONLY a phase
# manifest fragment (manifest-<phase>.json) — a phase driver NEVER advances
# a pointer and NEVER renames a directory. Any failed phase removes the
# whole shared staging set, so no mixed generation survives. After BOTH
# phases pass, the shell runs the final gates FIRST — web_sanity_http plus
# SIGINT (a `--listen` host is stdin-independent; closed stdin must not stop
# it) — and only
# then performs THE sole commit: it validates each fragment (exact
# lane/phase/nonce, unique declared artifact names as normalized relative
# paths contained in the run dir, every staged file an lstat regular file
# (no symlinks), exact-once counts, executed ids equal to the documented ids
# IN ORDER, disjoint documented id sets) and the EXACT full union (no
# dedupe, order-preserving),
# writes the merged coverage-assertions.json INSIDE the shared stage, then
# commits with ONE stage-dir -> run-dir rename ($evidence/runs/web-external-
# sessions-<nonce>/) plus ONE atomically-renamed current.json pointer. EVERY
# step — fragment parsing/validation, the merged-ledger write, the rename,
# and the pointer commit — runs inside ONE rollback scope: any validation
# failure or fault removes the shared stage/new run/temp and preserves the
# prior pointer byte-identical. The lane's invocation trap removes the shared
# staging set / new run dir / pointer temp on ANY setup, phase, or final-gate
# failure; the pass line prints ONLY after the sole commit succeeded.
#
# The native-only phase rewrites settings.json, kills the listener, and respawns so the
# Web catalog reloads under the authoritative empty import-sources policy.
# Foreign source files stay on disk (read-only contract); they must simply
# stop appearing in the sidebar.
#
# The lane FAILS (non-zero) whenever playwright/chromium cannot be used —
# RPI_WEB_FORCE_PLAYWRIGHT is irrelevant here because there is no fallback.
#
# Usage: bash E2E.d/web/external_sessions.sh [run|list]
set -euo pipefail
SCRIPT_DIR="$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
E2E_DIR="$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd -P)"
# shellcheck source=../lib/common.sh
. "$E2E_DIR/lib/common.sh"
# shellcheck source=lib/fixture.sh
. "$SCRIPT_DIR/lib/fixture.sh"

SCENARIO="web-external_sessions"
TOKEN="web-external_sessions-e2e-token-$$-$(date +%s)"


# Lane-set publication hygiene: the invocation trap removes THIS attempt's
# shared staging set, new run dir, and pointer temp file on ANY setup, phase,
# or final-gate failure (any non-zero exit before the sole lane-set commit),
# so a failed lane never leaves partial evidence. Once the sole commit
# succeeded (external_gate_committed=1) the published run dir + pointer are
# never touched by the trap.
external_gate_staging="" external_gate_run_dir="" external_gate_pointer_tmp=""
external_gate_committed=0
external_sessions_gate_cleanup() {
    [ -n "$external_gate_staging" ] && rm -rf -- "$external_gate_staging"
    [ -n "$external_gate_run_dir" ] && rm -rf -- "$external_gate_run_dir"
    [ -n "$external_gate_pointer_tmp" ] && rm -f -- "$external_gate_pointer_tmp"
    return 0
}
external_sessions_on_exit() {
    local status=$?
    if [ "$status" -ne 0 ] && [ "$external_gate_committed" -ne 1 ]; then
        external_sessions_gate_cleanup
    fi
    cleanup_e2e
}
external_sessions_on_signal() {
    [ "$external_gate_committed" -ne 1 ] && external_sessions_gate_cleanup
    cleanup_e2e
    trap - EXIT HUP INT TERM
    exit 1
}
# ---------------------------------------------------------------------------
main() {
    case "${1:-run}" in
        list|--list|--dry-run)
            printf '%s\n' \
                'web-external-sessions - Web default discovery+grouping across ALL provider groups (rpi/Pi/OMP/Codex/Grok), Agent-sessions collapse + bootstrapOnly hide/search/active-visibility, immutable Pi/Codex/OMP import+reuse, selected-row native/Pi HTML export without activation, failure toast/no download, sessionImportSources:[] native-only, zero browser errors, evidence published as ONE lane-set commit (one shared nonce staging set, one stage-dir->run-dir rename, one current.json pointer) with exact per-phase counts + exact full union (PLAYWRIGHT-ONLY, hard-fail)'
            return 0
            ;;
        run|all) ;;
        *) fail "usage: $0 [run|list]" ;;
    esac

    require_rpi
    # Playwright-only: node is REQUIRED. Missing node is a FAILURE, never a
    # skip and never a fallback to agent-browser.
    require_cmd node
    require_cmd python3
    require_cmd jq

    local root evidence port url pw_status=0 listen_port
    root="$(scenario_workspace "$SCENARIO")"
    git -C "$root/workspace" init -q
    git -C "$root/workspace" config user.email "web-external-sessions@example.com"
    git -C "$root/workspace" config user.name "Web External Sessions"
    git -C "$root/workspace" config commit.gpgsign false
    printf 'web external sessions seed\n' >"$root/workspace/seed.txt"
    git -C "$root/workspace" add -- seed.txt
    git -C "$root/workspace" commit -q -m "web external sessions seed"
    evidence="$EVIDENCE_ROOT/$SCENARIO"
    # ONE invocation nonce for the WHOLE lane-set: both phase drivers stage
    # into the SAME nonce-derived shared set and the shell commits it as ONE
    # publication (single stage->run rename + single current.json pointer).
    local lane_nonce="web-external-sessions-$$-$(date +%s)"
    local lane_staging="$evidence/.staging-web-external-sessions-$lane_nonce"
    external_gate_staging="$lane_staging"
    external_gate_run_dir="$evidence/runs/web-external-sessions-$lane_nonce"
    external_gate_pointer_tmp="$evidence/current.json.tmp-$lane_nonce"
    trap external_sessions_on_exit EXIT
    trap external_sessions_on_signal HUP INT TERM
    printf '%s\n' "$TOKEN" >"$root/token"

    # Absent sessionImportSources => Web default rpi+Pi+OMP+Codex+Grok.
    # scenario_workspace already wrote `{}`; keep that explicit for the lane.
    printf '{}\n' >"$root/home/.rpi/settings.json"

    # Seed one native row + four foreign sources under the isolated HOME.
    python3 - "$root/home" "$root/workspace" "$evidence" <<'PY'
import datetime, json, os, pathlib, sys

home = pathlib.Path(sys.argv[1])
cwd = sys.argv[2]
evidence = pathlib.Path(sys.argv[3])
agent = home / ".rpi"
sessions_root = agent / "sessions"

def encoded(path: str) -> str:
    s = path
    if s.startswith("/") or s.startswith("\\"):
        s = s[1:]
    return "--" + s.replace("/", "-").replace("\\", "-").replace(":", "-") + "--"

def write_text(path: pathlib.Path, body: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(body, encoding="utf-8")

stamp = datetime.datetime.now(datetime.timezone.utc).isoformat().replace("+00:00", "Z")

# Native seed (always eligible, including the native-only phase).
native_dir = sessions_root / encoded(cwd)
native_dir.mkdir(parents=True, exist_ok=True)
native_sid = "native-ext-seed"
native_file = native_dir / f"{stamp[:10]}T000000-{native_sid}.jsonl"
native_records = [
    {"type": "session", "version": 3, "id": native_sid, "timestamp": stamp, "cwd": cwd},
    # Explicit session_info name: the Web sidebar `temporary` marker must
    # never apply to this legal native seed (native + unnamed + <10KiB + cwd
    # under the OS temp root would otherwise hide it by default).
    {
        "type": "session_info",
        "id": "si-1",
        "parentId": None,
        "timestamp": stamp,
        "name": native_sid,
    },
    {
        "type": "message",
        "id": "m1",
        "parentId": None,
        "timestamp": stamp,
        "message": {
            "role": "user",
            "content": [{"type": "text", "text": "native-ext-seed"}],
            "timestamp": 1,
        },
    },
]
native_file.write_text(
    "".join(json.dumps(r, separators=(",", ":")) + "\n" for r in native_records),
    encoding="utf-8",
)

# Agent-subgroup seed: native session whose cwd sits under a Slock
# `.slock/agents/<id>` ephemeral agent workspace -> backend flags
# `agentSession` (visible by default, grouped under the default-collapsed
# "Agent sessions" subgroup). Named, so the temporary-workspace marker never
# applies.
agent_cwd = f"{home}/.slock/agents/agent-e2e"
agent_dir = sessions_root / encoded(agent_cwd)
agent_dir.mkdir(parents=True, exist_ok=True)
agent_sid = "agent-ext-e2e"
agent_file = agent_dir / f"{stamp[:10]}T000001-{agent_sid}.jsonl"
agent_records = [
    {"type": "session", "version": 3, "id": agent_sid, "timestamp": stamp, "cwd": agent_cwd},
    {
        "type": "session_info",
        "id": "si-agent-1",
        "parentId": None,
        "timestamp": stamp,
        "name": "agent-ext-seed",
    },
    {
        "type": "message",
        "id": "am1",
        "parentId": None,
        "timestamp": stamp,
        "message": {
            "role": "user",
            "content": [{"type": "text", "text": "agent workspace prompt"}],
            "timestamp": 1,
        },
    },
]
agent_file.write_text(
    "".join(json.dumps(r, separators=(",", ":")) + "\n" for r in agent_records),
    encoding="utf-8",
)

# Bootstrap-only seed: meaningful turns but every message is an injected
# bootstrap envelope -> backend `bootstrapOnly` -> hidden by default,
# searchable, forced visible while active/loaded. Explicitly named so the
# independent `temporary` view signal never also applies.
bootstrap_cwd = f"{cwd}/bootstrap-project"
bootstrap_dir = sessions_root / encoded(f"{cwd}/bootstrap-project")
bootstrap_dir.mkdir(parents=True, exist_ok=True)
bootstrap_sid = "bootstrap-ext-e2e"
bootstrap_file = bootstrap_dir / f"{stamp[:10]}T000002-{bootstrap_sid}.jsonl"
bootstrap_records = [
    {"type": "session", "version": 3, "id": bootstrap_sid, "timestamp": stamp, "cwd": f"{cwd}/bootstrap-project"},
    {
        "type": "session_info",
        "id": "si-b1",
        "parentId": None,
        "timestamp": stamp,
        "name": "bootstrap-ext-seed",
    },
    {
        "type": "message",
        "id": "b1",
        "parentId": None,
        "timestamp": stamp,
        "message": {
            "role": "user",
            "content": [{"type": "text", "text": "<environment_context>\nBootstrap context for the ephemeral agent\n</environment_context>"}],
            "timestamp": 1,
        },
    },
    {
        "type": "message",
        "id": "b2",
        "parentId": None,
        "timestamp": stamp,
        "message": {
            "role": "user",
            "content": "<user_info>bootstrap agent</user_info>",
            "timestamp": 2,
        },
    },
]
bootstrap_file.write_text(
    "".join(json.dumps(r, separators=(",", ":")) + "\n" for r in bootstrap_records),
    encoding="utf-8",
)

# Eight Pi sessions under ~/.pi/agent/sessions/. They are immutable foreign
# sources for rpi. The first row drives import/export assertions; all eight
# must render beneath the expanded Pi provider group.
pi_ids = ["pi-ext-e2e"] + [f"pi-ext-e2e-{index}" for index in range(2, 9)]
pi_files = []
for index, pi_name in enumerate(pi_ids):
    pi_file = home / ".pi" / "agent" / "sessions" / "--work--" / f"{pi_name}.jsonl"
    prompt = "Pi external prompt" if index == 0 else f"Pi external prompt {index + 1}"
    reply = "Pi external reply" if index == 0 else f"Pi external reply {index + 1}"
    pi_records = [
        {"type": "session", "version": 3, "id": pi_name, "timestamp": stamp, "cwd": cwd},
        {
            "type": "message",
            "id": f"pi-u-{index}",
            "parentId": None,
            "timestamp": f"2026-01-01T00:00:{index + 1:02d}Z",
            "message": {
                "role": "user",
                "content": [{"type": "text", "text": prompt}],
                "timestamp": index * 2 + 1,
            },
        },
        {
            "type": "message",
            "id": f"pi-a-{index}",
            "parentId": f"pi-u-{index}",
            "timestamp": f"2026-01-01T00:01:{index + 1:02d}Z",
            "message": {
                "role": "assistant",
                "content": [{"type": "text", "text": reply}],
                "timestamp": index * 2 + 2,
            },
        },
    ]
    write_text(
        pi_file,
        "".join(json.dumps(record, separators=(",", ":")) + "\n" for record in pi_records),
    )
    pi_files.append(pi_file)

pi_id = pi_ids[0]
pi_file = pi_files[0]

# OMP (pi-compatible v3 under ~/.omp/agent/sessions/--work--/).
# OMP rotates an oversized logical conversation into a fresh session file
# whose `session` header records the prior file's absolute path in
# `parentSession`; the leaf only holds the post-rotation turns plus a handoff
# custom message. Loading the leaf must concatenate the complete bounded
# chain (early -> middle -> final) as one ordered session.
omp_root = home / ".omp" / "agent" / "sessions" / "--work--"
# The leaf keeps the historical ompId ("omp-ext-e2e"); early/middle are its
# parentSession ancestors.
omp_ids = ["omp-ext-early", "omp-ext-mid", "omp-ext-e2e"]
omp_labels = ["early", "middle", "final"]
omp_paths = []
for index, (omp_name, label) in enumerate(zip(omp_ids, omp_labels)):
    parent = None if index == 0 else omp_paths[index - 1]
    records = []
    header = {
        "type": "session",
        "version": 3,
        "id": omp_name,
        "timestamp": f"2026-01-01T00:0{index}:00Z",
        "cwd": cwd,
    }
    if parent is not None:
        header["parentSession"] = str(parent)
    records.append(header)
    if index > 0:
        # OMP handoff custom message: never rendered as a user/assistant turn.
        records.append(
            {
                "type": "custom_message",
                "customType": "handoff",
                "content": f"handoff context summarizing {label} history",
                "display": True,
                "attribution": "agent",
                "id": f"handoff-{index}",
                "parentId": None,
                "timestamp": f"2026-01-01T00:0{index}:00.500Z",
            }
        )
    records.append(
        {
            "type": "message",
            "id": f"u{index}",
            "parentId": None if index == 0 else f"handoff-{index}",
            "timestamp": f"2026-01-01T00:0{index}:01Z",
            "message": {
                "role": "user",
                "content": [{"type": "text", "text": f"OMP {label} prompt"}],
                "timestamp": index * 2 + 1,
            },
        }
    )
    records.append(
        {
            "type": "message",
            "id": f"a{index}",
            "parentId": f"u{index}",
            "timestamp": f"2026-01-01T00:0{index}:02Z",
            "message": {
                "role": "assistant",
                "content": [{"type": "text", "text": f"OMP {label} reply"}],
                "timestamp": index * 2 + 2,
            },
        }
    )
    path = omp_root / f"{omp_name}.jsonl"
    write_text(
        path,
        "".join(json.dumps(record, separators=(",", ":")) + "\n" for record in records),
    )
    omp_paths.append(path)

omp_id = omp_ids[2]
omp_file = omp_paths[2]  # leaf is the selectable logical session

# Task/subagent child session at depth 3 under the parent session stem
# (`<dir>/<name>/<AgentId>.jsonl`): never listed, never chained.
omp_child_id = "omp-child-hidden"
omp_child_path = omp_root / "omp-ext-e2e" / f"{omp_child_id}.jsonl"
write_text(
    omp_child_path,
    (
        json.dumps(
            {
                "type": "session",
                "version": 3,
                "id": omp_child_id,
                "timestamp": "2026-01-01T00:06:00Z",
                "cwd": cwd,
            },
            separators=(",", ":"),
        )
        + "\n"
        + json.dumps(
            {
                "type": "message",
                "id": "cu",
                "parentId": None,
                "timestamp": "2026-01-01T00:06:01Z",
                "message": {
                    "role": "user",
                    "content": [{"type": "text", "text": "OMP child prompt"}],
                    "timestamp": 1,
                },
            },
            separators=(",", ":"),
        )
        + "\n"
    ),
)

# Codex rollout under ~/.codex/sessions/rollout-<id>.jsonl.
codex_id = "codex-ext-e2e"
codex_file = home / ".codex" / "sessions" / f"rollout-{codex_id}.jsonl"
write_text(
    codex_file,
    (
        json.dumps(
            {
                "type": "session_meta",
                "payload": {"id": codex_id, "cwd": cwd},
            },
            separators=(",", ":"),
        )
        + "\n"
        + json.dumps(
            {
                "type": "response_item",
                "payload": {
                    "type": "message",
                    "role": "user",
                    "content": [{"type": "input_text", "text": "Codex external prompt"}],
                },
            },
            separators=(",", ":"),
        )
        + "\n"
        + json.dumps(
            {
                "type": "response_item",
                "payload": {
                    "type": "message",
                    "role": "assistant",
                    "content": [{"type": "output_text", "text": "Codex external reply"}],
                },
            },
            separators=(",", ":"),
        )
        + "\n"
    ),
)

# Grok summary.json + chat_history.jsonl under ~/.grok/sessions/<cwd>/<id>/.
grok_id = "grok-ext-e2e"
grok_dir = home / ".grok" / "sessions" / "workspace" / grok_id
write_text(
    grok_dir / "summary.json",
    json.dumps(
        {
            "info": {"id": grok_id, "cwd": cwd},
            "created_at": "2026-01-01T00:00:00Z",
        },
        separators=(",", ":"),
    )
    + "\n",
)
write_text(
    grok_dir / "chat_history.jsonl",
    (
        json.dumps(
            {
                "role": "user",
                "content": "Grok external prompt",
                "timestamp": "2026-01-01T00:00:01Z",
            },
            separators=(",", ":"),
        )
        + "\n"
        + json.dumps(
            {
                "role": "assistant",
                "content": "Grok external reply",
                "timestamp": "2026-01-01T00:00:02Z",
            },
            separators=(",", ":"),
        )
        + "\n"
    ),
)

# Pin mtimes deterministically so boot restore prefers the native seed when
# the catalog is sorted newest-first (native newest). The depth-3 child is
# pinned newest of all to prove mtime never makes a child tree eligible.
base = native_file.stat().st_mtime
os.utime(native_file, (base + 3, base + 3))
os.utime(omp_file, (base + 2, base + 2))
os.utime(omp_paths[1], (base + 1.5, base + 1.5))
os.utime(codex_file, (base + 1, base + 1))
for index, pi_source in enumerate(pi_files):
    offset = 0.75 - index * 0.01
    os.utime(pi_source, (base + offset, base + offset))
os.utime(omp_paths[0], (base + 0.5, base + 0.5))
os.utime(omp_child_path, (base + 4, base + 4))
os.utime(grok_dir / "summary.json", (base, base))
os.utime(agent_file, (base + 0.25, base + 0.25))
os.utime(bootstrap_file, (base + 0.2, base + 0.2))

meta = {
    "cwd": cwd,
    "nativeSessionDir": str(native_dir),
    "nativeSessionsRoot": str(sessions_root),
    "nativeId": native_sid,
    "nativePath": str(native_file),
    "agentId": agent_sid,
    "agentCwd": agent_cwd,
    "agentPath": str(agent_file),
    "bootstrapId": bootstrap_sid,
    "bootstrapPath": str(bootstrap_file),
    "piId": pi_id,
    "piIds": pi_ids,
    "piPath": str(pi_file),
    "piPaths": [str(path) for path in pi_files],
    "ompId": omp_id,
    "ompPath": str(omp_file),
    "ompEarlyPath": str(omp_paths[0]),
    "ompEarlyId": omp_ids[0],
    "ompMidPath": str(omp_paths[1]),
    "ompMidId": omp_ids[1],
    "ompFinalPath": str(omp_paths[2]),
    "ompFinalId": omp_ids[2],
    "ompChildId": omp_child_id,
    "ompChildPath": str(omp_child_path),
    "codexId": codex_id,
    "codexPath": str(codex_file),
    "grokId": grok_id,
    "grokPath": str(grok_dir / "summary.json"),
    "grokChatPath": str(grok_dir / "chat_history.jsonl"),
}
(evidence / "seed-meta.json").write_text(json.dumps(meta, indent=2), encoding="utf-8")
print(json.dumps(meta))
PY

    # The suite has already built or verified one current-tree bundle. Keep
    # coverage's instrumented bundle and the normal production bundle alike.


    port="$(MOCK_SCENARIO=sessions web_start_mock "$root" "$evidence")"
    web_spawn_rpi "$root" "$evidence" "$port"
    url="$(web_wait_for_listener "$evidence")"
    log "$SCENARIO: listener at $url (default Web import sources)"

    # Capture the fixed listen port from the first banner so the native-only
    # respawn can keep the same /web URL for the second playwright phase.
    listen_port="$(printf '%s\n' "$url" | sed -n 's#.*://[^:]*:\([0-9]*\)/web#\1#p')"
    [ -n "$listen_port" ] || fail "$SCENARIO: could not parse listen port from $url"

    # Phase 1: default Web discovery + import/activation contracts.
    web_run_playwright "$url" "$evidence" "$root/playwright" "$SCRIPT_DIR/external_sessions_test.mjs" \
        "RPI_PHASE=default" \
        "RPI_LANE_NONCE=$lane_nonce" \
        "RPI_SEED_META=$evidence/seed-meta.json" \
        "RPI_NATIVE_SESSIONS_ROOT=$root/home/.rpi/sessions" \
        || pw_status=$?
    if [ "$pw_status" -ne 0 ]; then
        fail "$SCENARIO: playwright default phase failed (exit $pw_status) — external-sessions lane is playwright-only"
    fi

    # Phase 2: explicit empty sessionImportSources is native-only. Rewrite
    # settings, kill the listener, respawn on the same port (foreign files
    # remain on disk, read-only).
    printf '%s\n' '{"sessionImportSources":[]}' >"$root/home/.rpi/settings.json"
    web_kill_rpi
    sleep 0.3
    web_spawn_rpi "$root" "$evidence" "$port" "$listen_port" "2"
    url="$(web_wait_for_listener "$evidence" "2")"
    log "$SCENARIO: listener respawned at $url (sessionImportSources:[])"

    pw_status=0
    web_run_playwright "$url" "$evidence" "$root/playwright-native" "$SCRIPT_DIR/external_sessions_test.mjs" \
        "RPI_PHASE=native_only" \
        "RPI_LANE_NONCE=$lane_nonce" \
        "RPI_SEED_META=$evidence/seed-meta.json" \
        "RPI_NATIVE_SESSIONS_ROOT=$root/home/.rpi/sessions" \
        || pw_status=$?
    if [ "$pw_status" -ne 0 ]; then
        fail "$SCENARIO: playwright native-only phase failed (exit $pw_status) — external-sessions lane is playwright-only"
    fi
    # ONE lane-set commit over the shared nonce staging set. Validates, per
    # phase fragment, the exact lane/phase/nonce identity, that every
    # registered artifact is a CONTAINED RELATIVE path (no absolute paths,
    # no `..` escapes, nothing outside the future run dir), that executed
    # ids are exactly the documented ids with exact-once counts, and that
    # the two phases' documented id sets are DISJOINT — then writes
    # coverage-assertions.json as the EXACT full union (plain concatenation,
    # NO dedupe) INSIDE the shared stage, and commits with ONE stage-dir ->
    # run-dir rename plus ONE atomically-renamed current.json pointer. A
    # pre-pointer-commit failure removes the staging dir / renamed run dir /
    # pointer temp file and preserves the previous pointer byte-identical —
    # never an orphan run dir, never a partial or mixed generation.
    # Final gates BEFORE the sole publication commit: the listener must
    # still serve / (web_sanity_http). `--listen` is stdin-independent
    # (`pty_listener_is_headless_and_signal_owned_with_closed_stdin`); SIGINT
    # requests host shutdown. A lane that cannot close its listener never
    # publishes evidence. web_finish_lane's close+print pair is split here:
    # the close/listener gates run before the commit, the pass line prints
    # only after it.
    web_sanity_http "$url"
    [ -n "${RPI_PID:-}" ] || fail "$SCENARIO: listener PID missing before the close gate"
    web_close_rpi_stdin
    log "$SCENARIO: stopping the listener with SIGINT and verifying exit"
    kill -INT "$RPI_PID"
    local close_tries=0
    while [ "$close_tries" -lt 25 ] && kill -0 "$RPI_PID" 2>/dev/null; do
        sleep 0.2
        close_tries=$((close_tries + 1))
    done
    if kill -0 "$RPI_PID" 2>/dev/null; then
        fail "$SCENARIO: listener did not exit after SIGINT (close gate failed)"
    fi
    wait "$RPI_PID" 2>/dev/null || true
    RPI_PID=""
    sleep 0.5

    python3 - "$evidence" "$lane_nonce" "${2:-commit}" <<'PY'
import json, os, pathlib, shutil, stat, sys, tempfile

LANE = "web-external-sessions"
PHASES = ("default", "native_only")


def bail(message):
    raise SystemExit(f"web-external-sessions: lane-set commit FAILED: {message}")


def commit_lane_set(evidence, nonce, fault_at=None):
    """THE production lane-set publication helper.

    Validates the shared nonce staging set (exact lane/phase/nonce
    identity per fragment, unique declared artifact names, normalized
    relative paths contained inside the run dir, lstat REGULAR staged
    files only — symlinks rejected, exact-once counts, disjoint
    documented id sets, exact full union with NO dedupe), writes the
    merged coverage-assertions.json INSIDE the shared stage, renames the
    stage ONCE into the immutable run dir, and commits ONE
    atomically-renamed current.json pointer. EVERY step — fragment
    parsing/validation, the merged-ledger write, the stage->run rename,
    and the pointer tmp write + rename — runs INSIDE one rollback scope:
    any fault, validation failure, or real failure BEFORE the pointer
    commit removes the staging dir, the renamed run dir, and the pointer
    temp file, and preserves the previous pointer byte-identical.
    `fault_at` injects a failure at one pre-pointer-commit stage —
    SELF-CHECK ONLY; production callers never pass it:
      'manifest'       after the fragments validate, before the stage rename
      'rename'         after the stage->run rename, before the pointer write
      'pointer-write'  after the pointer temp write, before its rename
      'pointer-rename' immediately before the pointer rename commit
    """
    evidence = pathlib.Path(evidence)
    staging = evidence / f".staging-{LANE}-{nonce}"
    run_dir_name = f"runs/web-external-sessions-{nonce}"
    run_dir = evidence / run_dir_name
    pointer_path = evidence / "current.json"
    pointer_tmp = evidence / f"current.json.tmp-{nonce}"
    renamed = False
    try:
        if not staging.is_dir():
            bail(f"shared lane-set staging dir missing: {staging}")

        merged = []  # exact full union: plain concatenation, never deduped
        documented_union = []  # disjoint per-phase documented lists, concatenated
        seen = set()
        phase_pointers = []
        fragments = {}
        for phase in PHASES:
            fragment_path = staging / f"manifest-{phase}.json"
            if not fragment_path.is_file():
                bail(f"missing phase manifest fragment: {fragment_path}")
            try:
                fragment = json.loads(fragment_path.read_text(encoding="utf-8"))
            except (OSError, ValueError) as err:
                bail(f"{phase}: phase manifest fragment is not readable JSON: {err}")
            if not isinstance(fragment, dict):
                bail(f"{phase}: fragment is not a JSON object")
            if fragment.get("lane") != LANE:
                bail(f"{phase}: fragment lane mismatch: {fragment.get('lane')}")
            if fragment.get("phase") != phase:
                bail(f"{phase}: fragment phase mismatch: {fragment.get('phase')}")
            if fragment.get("nonce") != nonce:
                bail(f"{phase}: fragment nonce mismatch (mixed generation forbidden): {fragment.get('nonce')}")
            documented = fragment.get("documented") or []
            executed = fragment.get("executed") or []
            counts = fragment.get("counts") or {}
            artifacts = fragment.get("artifacts") or []
            for field, value in (("documented", documented), ("executed", executed), ("artifacts", artifacts)):
                if not isinstance(value, list) or any(not isinstance(item, str) or not item for item in value):
                    bail(f"{phase}: fragment {field} is not a list of non-empty strings")
            if not isinstance(counts, dict):
                bail(f"{phase}: fragment counts is not an object: {json.dumps(counts, sort_keys=True)}")
            if len(set(documented)) != len(documented):
                bail(f"{phase}: documented ids contain duplicates: {documented}")
            # ORDERED equality — never a sorted-set comparison, which would
            # silently accept reordered executed ids masking real drift
            # between the documented order and the actual assertion order.
            if executed != documented:
                bail(
                    f"{phase}: executed ids are not exactly the documented ids IN ORDER "
                    f"(documented={documented}, executed={executed})"
                )
            if len(counts) != len(documented) or any(counts.get(i) != 1 for i in documented):
                bail(f"{phase}: counts are not exact-once: {json.dumps(counts, sort_keys=True)}")
            declared = set()
            for name in artifacts:
                pure = pathlib.PurePosixPath(name)
                if pure.is_absolute() or ".." in pure.parts or "." in pure.parts or str(pure) != name:
                    bail(f"{phase}: registered artifact escapes the run dir (must be a normalized contained relative path): {name!r}")
                if name in declared:
                    bail(f"{phase}: duplicate declared artifact name: {name!r}")
                declared.add(name)
            overlap = sorted(set(documented) & seen)
            if overlap:
                bail(f"{phase}: assertion ids overlap earlier phases: {overlap}")
            seen.update(documented)
            documented_union.extend(documented)
            merged.extend(executed)
            phase_pointers.append({
                "phase": phase,
                "manifest": f"manifest-{phase}.json",
                "documented": len(documented),
                "executed": len(executed),
            })

        # Exact artifact set: the shared stage must contain EXACTLY the union
        # of every fragment's registered artifacts plus the two manifest
        # fragments — nothing missing, nothing extra, no stray staging
        # residue, and every staged entry an lstat REGULAR file (a symlink —
        # even one pointing at a real file — is rejected).
        expected = set()
        for phase in PHASES:
            fragment = json.loads((staging / f"manifest-{phase}.json").read_text(encoding="utf-8"))
            expected.update(fragment.get("artifacts") or [])
            expected.add(f"manifest-{phase}.json")
        actual = {}
        def scan_stage(dir_path, prefix):
            for entry in os.scandir(dir_path):
                rel = f"{prefix}{entry.name}"
                stat_result = os.lstat(entry.path)
                if stat.S_ISDIR(stat_result.st_mode):
                    scan_stage(entry.path, f"{rel}/")
                elif stat.S_ISREG(stat_result.st_mode):
                    actual[rel] = True
                else:
                    bail(f"staged entry is not a regular file (symlinks/special files rejected): {rel!r}")
        scan_stage(staging, "")
        missing = sorted(expected - set(actual))
        extra = sorted(set(actual) - expected)
        if missing or extra:
            bail(f"exact staged-set violation: missing={missing}, unexpected={extra}")

        union_counts = {}
        for assertion_id in merged:
            union_counts[assertion_id] = union_counts.get(assertion_id, 0) + 1
        if len(union_counts) != len(merged):
            bail("exact full union violated: merged ids contain duplicates (dedupe is forbidden)")

        # The merged coverage ledger is written INSIDE the shared stage, so
        # the single rename publishes the complete set atomically. The
        # documented union is the plain concatenation of the disjoint
        # per-phase documented lists — the executed union MUST equal it.
        # ORDERED equality: the executed union must equal the documented
        # union element-for-element in order (each phase fragment is already
        # order-gated above; this re-checks the concatenation).
        if merged != documented_union:
            bail("merged ledger mismatch: executed union is not the documented union IN ORDER")
        (staging / "coverage-assertions.json").write_text(
            json.dumps(
                {"documented": documented_union, "executed": merged, "counts": union_counts},
                indent=2,
            ) + "\n",
            encoding="utf-8",
        )

        if fault_at == "manifest":
            raise OSError("injected: manifest/merge failure")
        if run_dir.exists():
            bail(f"run dir already exists (nonce collision = mixed generation): {run_dir}")
        (evidence / "runs").mkdir(parents=True, exist_ok=True)
        if fault_at == "rename":
            raise OSError("injected: stage->run rename failure")
        os.rename(staging, run_dir)
        renamed = True
        pointer = {
            "lane": LANE,
            "nonce": nonce,
            "run": run_dir_name,
            "manifests": [entry["manifest"] for entry in phase_pointers],
            "coverage": "coverage-assertions.json",
            "phases": phase_pointers,
        }
        if fault_at == "pointer-write":
            raise OSError("injected: pointer write failure")
        pointer_tmp.write_text(json.dumps(pointer, indent=2) + "\n", encoding="utf-8")
        if fault_at == "pointer-rename":
            raise OSError("injected: pointer rename failure")
        os.replace(pointer_tmp, pointer_path)
    except BaseException:
        # Pre-pointer-commit failure (injected fault, validation failure, or
        # real failure): remove THIS attempt's staging dir, renamed run dir,
        # and pointer temp file; the previous pointer (if any) keeps naming
        # the previous complete publication.
        shutil.rmtree(staging, ignore_errors=True)
        if renamed:
            shutil.rmtree(run_dir, ignore_errors=True)
        try:
            pointer_tmp.unlink()
        except FileNotFoundError:
            pass
        raise

    return {
        "run_dir_name": run_dir_name,
        "merged": len(merged),
        "pointer": pointer_path,
    }


mode = sys.argv[3] if len(sys.argv) > 3 else "commit"


def seed_staging(root, nonce):
    staging = root / f".staging-{LANE}-{nonce}"
    staging.mkdir(parents=True, exist_ok=True)
    for phase in PHASES:
        (staging / f"probe-{phase}.json").write_text('{"probe":1}\n', encoding="utf-8")
        (staging / f"manifest-{phase}.json").write_text(
            json.dumps({
                "lane": LANE,
                "phase": phase,
                "nonce": nonce,
                "documented": [f"{phase}-1"],
                "executed": [f"{phase}-1"],
                "counts": {f"{phase}-1": 1},
                "artifacts": [f"probe-{phase}.json"],
            }, indent=2) + "\n",
            encoding="utf-8",
        )
    return staging


def run_self_check():
    # Self-check over the ACTUAL helper (not a reimplementation): builds a
    # throwaway root, seeds a previous pointer + a complete shared staging
    # set for both phases, and then (a) rejects every INVALID staged set —
    # duplicate declared artifact name, missing declared artifact, extra
    # unregistered file, symlinked artifact (even to a real file), `..`
    # traversal name, wrong lane, wrong nonce, wrong phase — and (b) injects
    # a fault at every pre-pointer-commit stage. EVERY rejected or injected
    # run must raise, leave NO staging dir / renamed run dir / pointer temp
    # file behind, and preserve the previous pointer byte-identical; a clean
    # invocation then publishes for real (run dir materializes, pointer
    # atomically replaced, no temp residue).

    def reset():
        shutil.rmtree(staging, ignore_errors=True)
        shutil.rmtree(root / "runs", ignore_errors=True)
        (root / f"current.json.tmp-{nonce}").unlink(missing_ok=True)
        pointer_path.write_text(sentinel, encoding="utf-8")
        seed_staging(root, nonce)

    def assert_rollback(label):
        if staging.exists():
            bail(f"self-check: staging dir survived {label}")
        if (root / f"runs/web-external-sessions-{nonce}").exists():
            bail(f"self-check: renamed run dir survived {label}")
        if any(path.name.startswith("current.json.tmp-") for path in root.iterdir()):
            bail(f"self-check: pointer temp survived {label}")
        if pointer_path.read_text(encoding="utf-8") != sentinel:
            bail(f"self-check: previous pointer mutated by {label}")

    def must_fail(label):
        threw = False
        try:
            commit_lane_set(root, nonce)
        except (OSError, SystemExit):
            threw = True
        if not threw:
            bail(f"self-check: invalid lane-set {label} did not raise")
        assert_rollback(label)
    def fragment_path(phase):
        return staging / f"manifest-{phase}.json"

    def rewrite_fragment(phase, mutate):
        fragment = json.loads(fragment_path(phase).read_text(encoding="utf-8"))
        mutate(fragment)
        fragment_path(phase).write_text(json.dumps(fragment, indent=2) + "\n", encoding="utf-8")

    root = pathlib.Path(tempfile.mkdtemp(prefix="web-external-sessions-commit-selfcheck-"))
    try:
        nonce = "self-check-nonce"
        staging = root / f".staging-{LANE}-{nonce}"
        pointer_path = root / "current.json"
        sentinel = '{"sentinel":"previous publication"}\n'
        # ---- invalid staged sets / fragments: every one must be rejected
        # with a full rollback and the previous pointer preserved ----
        invalid_cases = {
            "duplicate declared artifact": lambda: rewrite_fragment(
                "default", lambda fragment: fragment.update(artifacts=["probe-default.json", "probe-default.json"])),
            "missing declared artifact": lambda: rewrite_fragment("native_only", lambda fragment: fragment.update(artifacts=["ghost.json"])),
            "extra staged file": lambda: (staging / "stray.json").write_text('{"stray":1}\n', encoding="utf-8"),
            "symlinked artifact": lambda: (
                (root / "outside-target.txt").write_text("real bytes\n", encoding="utf-8"),
                (staging / "probe-default.json").unlink(),
                os.symlink(root / "outside-target.txt", staging / "probe-default.json"),
            ),
            "traversal artifact name": lambda: rewrite_fragment("default", lambda fragment: fragment.update(artifacts=["../escape.json"])),
            "absolute artifact path": lambda: rewrite_fragment("default", lambda fragment: fragment.update(artifacts=["/etc/passwd"])),
            "wrong fragment lane": lambda: rewrite_fragment("default", lambda fragment: fragment.update(lane="web-other-lane")),
            "wrong fragment nonce": lambda: rewrite_fragment("default", lambda fragment: fragment.update(nonce="other-generation")),
            "executed/documented mismatch": lambda: rewrite_fragment("default", lambda fragment: fragment.update(executed=["default-2"])),
            "duplicate documented ids": lambda: rewrite_fragment("default", lambda fragment: fragment.update(documented=["default-1", "default-1"], executed=["default-1", "default-1"], counts={"default-1": 1})),
            "wrong fragment phase": lambda: rewrite_fragment("default", lambda fragment: fragment.update(phase="native_only")),
            "reordered executed ids": lambda: rewrite_fragment("default", lambda fragment: fragment.update(documented=["default-1", "default-2"], executed=["default-2", "default-1"], counts={"default-1": 1, "default-2": 1})),
        }
        for label, mutate_stage in invalid_cases.items():
            reset()
            mutate_stage()
            must_fail(label)
        # ---- fault injection at every pre-pointer-commit stage ----
        for fault_at in ("manifest", "rename", "pointer-write", "pointer-rename"):
            reset()
            threw = False
            try:
                commit_lane_set(root, nonce, fault_at=fault_at)
            except (OSError, SystemExit):
                threw = True
            if not threw:
                bail(f"self-check: fault {fault_at} did not raise")
            assert_rollback(f"fault {fault_at}")
        # Clean invocation: the run dir materializes and the pointer names it.
        reset()
        result = commit_lane_set(root, nonce)
        run_dir = root / result["run_dir_name"]
        if staging.exists():
            bail("self-check: staging dir survived a successful lane-set commit")
        if not (run_dir / "coverage-assertions.json").is_file():
            bail("self-check: merged coverage ledger missing from the committed run dir")
        pointer = json.loads(pointer_path.read_text(encoding="utf-8"))
        if pointer.get("lane") != LANE or pointer.get("nonce") != nonce \
                or pointer.get("run") != result["run_dir_name"] \
                or pointer.get("coverage") != "coverage-assertions.json" \
                or pointer.get("manifests") != [f"manifest-{phase}.json" for phase in PHASES] \
                or len(pointer.get("phases") or []) != len(PHASES):
            bail(f"self-check: pointer does not name the new lane-set run: {json.dumps(pointer)}")
        if any(path.name.startswith("current.json.tmp-") for path in root.iterdir()):
            bail("self-check: pointer temp survived a successful lane-set commit")
        ledger = json.loads((run_dir / "coverage-assertions.json").read_text(encoding="utf-8"))
        if ledger.get("documented") != ["default-1", "native_only-1"] \
                or ledger.get("executed") != ["default-1", "native_only-1"] \
                or ledger.get("counts") != {"default-1": 1, "native_only-1": 1}:
            bail(f"self-check: merged ledger is not the exact documented/executed/counts union: {json.dumps(ledger)}")
    finally:
        shutil.rmtree(root, ignore_errors=True)
    print("web-external-sessions: lane-set commit self-check PASS (invalid/duplicate/missing/extra/symlink/traversal rejected, all four fault points, previous pointer preserved)")


if mode == "self-check":
    run_self_check()
    sys.exit(0)

# Every REAL lane-set commit runs the fault-injection self-check over the
# actual helper first — a commit helper that cannot self-verify its
# failure-clean rollback must not publish.
run_self_check()
result = commit_lane_set(sys.argv[1], sys.argv[2])
print(f"merged {result['merged']} assertion ids (exact full union, no dedupe)")
print(f"lane-set commit: {result['run_dir_name']} (pointer: {result['pointer']})")
PY

    external_gate_committed=1
    printf '%s passed\nevidence=%s\n' "$SCENARIO" "$EVIDENCE_ROOT/$SCENARIO"
}

main "$@"
