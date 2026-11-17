#!/usr/bin/env bash
set -euo pipefail
SCRIPT_DIR="$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
E2E_DIR="$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd -P)"
# shellcheck source=../lib/common.sh
. "$E2E_DIR/lib/common.sh"
# shellcheck source=../lib/orchestration_fixtures.sh
. "$E2E_DIR/lib/orchestration_fixtures.sh"

SCENARIO="code-review-ops"
PROVIDER="code-review-ops"
MODEL_ID="mock"
EARLY_MARKER="EARLY-HUNK-FLAG-0012"
MIDDLE_TAG="OPS-PADMIDDLE"
DEEP_MARKER="DEEP-HUNK-MARK-0240"
DEEP_SECOND="OPS-DEEP-NEXT-0241"
DEEP_HEADER='@@ -237,7 +1507,8 @@'
COMMENT_SENTINEL="OPS-COMMENT-SENTINEL-01"
SWITCH_MODEL="ops-switch-model"
AUTO_CARD="Auto review"
MANUAL_REPLY="MANUAL-REPLY-OPS-BODY"
CRITICAL_SUMMARY="Critical: deep marker has no guard"
CRITICAL_EVIDENCE="DEEP-HUNK-MARK-0240"
SECOND_SUMMARY="Naming: second deep row"
OVERDESIGN_SUMMARY="Overdesign: middle pad duplicates state"
NAMING_SUMMARY="Naming: deleted early baseline"
TLDR_OVERVIEW="Pinned lib.rs and z_big.md change the review base."

wait_for() { # needle [timeout]
    local needle="$1" timeout="${2:-25}"
    if ! tmux_wait_for "$session" "$timeout" "$needle" >"$evidence/live.txt"; then
        fail "user.$SCENARIO: did not find ${needle@Q} (see $evidence/live.txt)"
    fi
}

wait_all() { # timeout needle...
    local timeout="$1"
    shift
    if ! tmux_wait_for "$session" "$timeout" "$@" >"$evidence/live.txt"; then
        fail "user.$SCENARIO: did not find one of [$*] (see $evidence/live.txt)"
    fi
}

pane_capture() {
    tmux capture-pane -p -S -2000 -t "$session":0 2>/dev/null || fail "user.$SCENARIO: tmux pane capture failed"
}

capture_to() { # path
    pane_capture >"$1"
}

wait_gone() { # needle timeout label
    local needle="$1" timeout="$2" label="$3" deadline capture
    deadline=$((SECONDS + timeout))
    while ((SECONDS < deadline)); do
        capture="$(pane_capture)"
        case "$capture" in
            *"$needle"*) sleep 0.25 ;;
            *)
                printf '%s\n' "$capture" >"$evidence/gone-$label.txt"
                return 0
                ;;
        esac
    done
    capture_to "$evidence/gone-$label.txt"
    fail "user.$SCENARIO: ${needle@Q} never cleared (see $evidence/gone-$label.txt)"
}

assert_never_appears() { # label timeout needle...
    local label="$1" timeout="$2" deadline capture needle
    shift 2
    deadline=$((SECONDS + timeout))
    while ((SECONDS < deadline)); do
        capture="$(pane_capture)"
        for needle in "$@"; do
            case "$capture" in
                *"$needle"*)
                    printf '%s\n' "$capture" >"$evidence/never-$label.txt"
                    fail "user.$SCENARIO: ${needle@Q} appeared unexpectedly in the $label window (see $evidence/never-$label.txt)"
                    ;;
            esac
        done
        sleep 0.25
    done
    capture_to "$evidence/never-$label.txt"
}

release_gate() { # kind
    curl -s --max-time 5 -X POST "http://127.0.0.1:${mock_port}/__release-$1" \
        -d '{}' >"$evidence/release-$1.txt"
}

release_gate_to() { # kind path
    curl -s --max-time 5 -X POST "http://127.0.0.1:${mock_port}/__release-$1" \
        -d '{}' >"$2"
}

advance_summary_generation() {
    curl -s --max-time 5 -X POST "http://127.0.0.1:${mock_port}/__tldr-generation" \
        -d '{}' >"$evidence/tldr-generation.txt"
}


log_count() { # fixed-string -> occurrences in the mock log
    awk -v pattern="$1" 'index($0, pattern) {count++} END {print count + 0}' "$evidence/mock-server.log"
}

log_count_end() { # classification that must end the log line
    awk -v pattern="$1" '$0 ~ ("(^| )" pattern "$") {count++} END {print count + 0}' "$evidence/mock-server.log"
}

require_mock_count() { # fixed-string expected
    local got
    got="$(log_count "$1")"
    [ "$got" -eq "$2" ] || fail "user.$SCENARIO: mock log expected $2 of ${1@Q}, got $got (see $evidence/mock-server.log)"
}

require_mock_count_end() { # end-anchored classification expected
    local got
    got="$(log_count_end "$1")"
    [ "$got" -eq "$2" ] || fail "user.$SCENARIO: mock log expected $2 lines ending ${1@Q}, got $got (see $evidence/mock-server.log)"
}

wait_log() { # needle minimum-logged-count [timeout]
    local needle="$1" want="$2" timeout="${3:-10}" deadline
    deadline=$((SECONDS + timeout))
    while ((SECONDS < deadline)); do
        if [ "$(log_count "$needle")" -ge "$want" ]; then
            return 0
        fi
        sleep 0.25
    done
    capture_to "$evidence/wait-log-timeout.txt"
    fail "user.$SCENARIO: mock log never reached $want of ${needle@Q} within ${timeout}s (see $evidence/mock-server.log and $evidence/wait-log-timeout.txt)"
}

assert_mock_counts() {
    require_mock_count 'kind=triage' 2
    require_mock_count 'kind=auto' 1
    require_mock_count 'kind=highlight' 4
    require_mock_count 'kind=manual' 1
    require_mock_count 'kind=unrouted' 0
    require_mock_count 'kind=tldr' 6
    require_mock_count 'tldr contract=purpose-json' 6
    require_mock_count_end 'tldr contract=old-protocol' 0
    require_mock_count_end 'tldr contract=old-protocol-absent' 6
    require_mock_count 'tldr generation=1' 1
    require_mock_count 'tldr generation=2' 5
    require_mock_count 'strict=ok' 2
    require_mock_count 'findings-schema=ok' 4
    require_mock_count 'auto-schema=absent' 1
    require_mock_count 'lang=zh-CN' 5
    require_mock_count 'lang=missing' 0
    require_mock_count 'auto-scope=1' 1
    require_mock_count 'highlight-scope=complete' 4
    require_mock_count 'highlight-invalid-range' 1
    require_mock_count 'release=triage held=true' 2
    require_mock_count 'release=highlight held=true' 3
    require_mock_count 'release=auto held=true' 1
    require_mock_count 'held=false' 0
}

assert_large_summary_proof() { # expected-patches.json
    python3 - "$evidence/mock-server.log" "$evidence/rpi-stderr.log" "$1" <<'PY'
import json
import pathlib
import re
import sys

def fail(message):
    raise SystemExit(f"large-summary proof: {message}")

mock = pathlib.Path(sys.argv[1]).read_text(encoding="utf-8", errors="replace").splitlines()
stderr = pathlib.Path(sys.argv[2]).read_text(encoding="utf-8", errors="replace").splitlines()
expected = json.loads(pathlib.Path(sys.argv[3]).read_text(encoding="utf-8"))["patches"]
GEN = re.compile(r"tldr generation=(\d+) batch=(\d+)$")
BYTES = re.compile(r"tldr prompt_bytes=(\d+) prompt_digest=([0-9a-f]{64})$")
LARGE = re.compile(r"tldr-large file=(\S+) sha256=([0-9a-f]{64}) bytes=(\d+) head=1 middle=1 tail=1$")
COVER = re.compile(r"tldr-large coverage=complete files=(\d+)$")
START = re.compile(r"summary_batch category=start batch=(\d+)/(\d+) designated_keys=(\d+) estimated_input_bytes=(\d+)$")

requests, current, coverage = [], None, []
for raw in mock:
    line = raw.strip()
    match = GEN.search(line)
    if match:
        current = {"generation": int(match.group(1)), "batch": int(match.group(2)),
                   "bytes": None, "digest": None, "large": {}}
        requests.append(current)
        continue
    match = BYTES.search(line)
    if match:
        if current is None:
            fail("prompt bytes without a request header")
        current["bytes"], current["digest"] = int(match.group(1)), match.group(2)
        continue
    match = LARGE.search(line)
    if match:
        if current is None:
            fail("large record without a request header")
        path = match.group(1)
        if path in current["large"]:
            fail(f"{path} repeated within one batch")
        current["large"][path] = (match.group(2), int(match.group(3)))
        continue
    match = COVER.search(line)
    if match:
        coverage.append(int(match.group(1)))

starts = []
for raw in stderr:
    match = START.search(raw.strip())
    if match:
        starts.append((int(match.group(2)), int(match.group(4))))

if len(requests) != 6:
    fail(f"expected 6 TLDR requests, saw {len(requests)}")
if [request["generation"] for request in requests] != [1, 2, 2, 2, 2, 2]:
    fail("generation sequence")
if [request["batch"] for request in requests] != [1, 1, 2, 3, 4, 5]:
    fail("batch sequence")
if any(request["bytes"] is None or not request["digest"] for request in requests):
    fail("missing prompt byte/digest trace")
if len(starts) != len(requests):
    fail(f"stderr starts {len(starts)} != requests {len(requests)}")
if any(denominator != 5 for denominator, _ in starts):
    fail("batch denominator is not 5")
if [size for _, size in starts] != [request["bytes"] for request in requests]:
    fail("stderr estimated_input_bytes differ from received prompt bytes")

large = [request for request in requests if request["large"]]
if len(large) != 2:
    fail(f"expected 2 large batches, saw {len(large)}")
if any(len(request["large"]) != 1 for request in large):
    fail("a large batch carried more than one record")
seen = {}
for request in large:
    for path, (digest, size) in request["large"].items():
        entry = expected.get(path)
        if entry is None:
            fail(f"unexpected large path {path}")
        if digest != entry["sha256"] or size != entry["bytes"]:
            fail(f"{path} digest/bytes differ from the git-derived expectation")
        if size >= 2 * 1024 * 1024:
            fail(f"{path} patch body is not below 2 MiB")
        seen[path] = request
if sorted(seen) != sorted(expected):
    fail(f"expected files seen {sorted(seen)} != {sorted(expected)}")
if large[0]["digest"] == large[1]["digest"]:
    fail("large prompts have the same digest")
if sum(request["bytes"] for request in large) <= 2 * 1024 * 1024:
    fail("large prompt bytes do not exceed 2 MiB combined")
if any(request["bytes"] >= 2 * 1024 * 1024 for request in large):
    fail("a large prompt reached the 2 MiB cap")
if coverage != [len(expected)]:
    fail(f"coverage marker {coverage} != [{len(expected)}]")
PY
}

seed_review_fixture() { # root
    local root="$1" workspace="$1/workspace" status
    local gitenv=(env GIT_CONFIG_NOSYSTEM=1 GIT_CONFIG_GLOBAL="$root/absent-global-gitconfig")
    require_cmd git
    require_cmd python3
    "${gitenv[@]}" git -C "$workspace" init -q
    "${gitenv[@]}" git -C "$workspace" config user.email tui-e2e@example.com
    "${gitenv[@]}" git -C "$workspace" config user.name "TUI E2E"
    "${gitenv[@]}" git -C "$workspace" config commit.gpgsign false
    printf 'review root\n' >"$workspace/README.md"
    python3 - "$workspace/src/lib.rs" "$workspace/z_big.md" <<'PY'
import pathlib
import sys
base = "".join(f"LIB-BASE-{n:03d}\n" for n in range(1, 261))
pathlib.Path(sys.argv[1]).parent.mkdir(parents=True, exist_ok=True)
pathlib.Path(sys.argv[1]).write_text(base, encoding="utf-8")
pathlib.Path(sys.argv[2]).write_text("ZBIG-BASELINE\n", encoding="utf-8")
PY
    "${gitenv[@]}" git -C "$workspace" add -A
    "${gitenv[@]}" git -C "$workspace" commit -q -m "ops review base"
    "${gitenv[@]}" git -C "$workspace" branch -m review-base
    python3 - "$workspace/src/lib.rs" "$workspace/z_big.md" \
        "$EARLY_MARKER" "$MIDDLE_TAG" "$DEEP_MARKER" "OPS-DEEPPAD" "OPS-ZBIG" <<'PY'
import pathlib
import sys
lib, big = pathlib.Path(sys.argv[1]), pathlib.Path(sys.argv[2])
lines = [f"LIB-BASE-{n:03d}\n" for n in range(1, 261)]
lines[11] = sys.argv[3] + " unique-early-flag-line\n"
lines[239] = sys.argv[5] + " unique-deep-flag-line\n"
lines.insert(240, "OPS-DEEP-NEXT-0241 unique-deep-adjacent-line\n")
lines[30:210] = [f"{sys.argv[4]}-{i:04d} unique-middle-pad-row\n" for i in range(1450)]
lines.extend(f"{sys.argv[6]}-{i:05d} " + "p" * 78 + "\n" for i in range(30000))
lib.write_text("".join(lines), encoding="utf-8")
big.write_text(
    "".join(f"{sys.argv[7]}-{i:05d} " + "z" * 78 + "\n" for i in range(26000)),
    encoding="utf-8",
)
PY
    grep -F -q "$EARLY_MARKER" "$workspace/src/lib.rs" || fail "fixture: $EARLY_MARKER missing"
    grep -F -q "$DEEP_MARKER" "$workspace/src/lib.rs" || fail "fixture: $DEEP_MARKER missing"
    grep -F -q "$DEEP_SECOND" "$workspace/src/lib.rs" || fail "fixture: $DEEP_SECOND missing"
    [ "$(wc -c <"$workspace/z_big.md")" -gt 2000000 ] || fail "fixture: z_big.md too small for the truncation placeholder"
    status="$("${gitenv[@]}" git -C "$workspace" status --porcelain)"
    case "$status" in
        *" M src/lib.rs"*) ;;
        *) fail "fixture: src/lib.rs not modified vs review-base: ${status:-<empty>}" ;;
    esac
    case "$status" in
        *" M z_big.md"*) ;;
        *) fail "fixture: z_big.md not modified vs review-base: ${status:-<empty>}" ;;
    esac
    seed_summary_fixture "$root" "$workspace"
    case "$("${gitenv[@]}" git -C "$workspace" status --porcelain=v1)" in
        *" M src/lib.rs"*|*"MM src/lib.rs"*) ;;
        *) fail "fixture: src/lib.rs lost its unstaged edit" ;;
    esac
}

seed_summary_fixture() { # parent workspace
    local parent="$1" workspace="$2"
    local gitenv=(env GIT_CONFIG_NOSYSTEM=1 GIT_CONFIG_GLOBAL="$parent/absent-global-gitconfig")
    local ahead head_oid origin_oid both_staged both_unstaged origin big="$workspace/big"
    # Summary ground truth. The primary stays one commit ahead of a local
    # bare origin. both.rs is MM. kept.txt is deleted. old-name.txt ->
    # new-name.txt is a rename. pixel.png is a committed binary change.
    # loose.txt is untracked. bare/ has no origin. clean/ matches its own
    # local bare origin.
    mkdir -p "$parent" "$workspace" "$workspace/bare" "$workspace/clean"
    if ! "${gitenv[@]}" git -C "$workspace" rev-parse --is-inside-work-tree >/dev/null 2>&1; then
        "${gitenv[@]}" git -C "$workspace" init -q
        "${gitenv[@]}" git -C "$workspace" config user.email tui-e2e@example.com
        "${gitenv[@]}" git -C "$workspace" config user.name "TUI E2E"
        "${gitenv[@]}" git -C "$workspace" config commit.gpgsign false
        printf 'summary root\n' >"$workspace/README.md"
        "${gitenv[@]}" git -C "$workspace" add -A
        "${gitenv[@]}" git -C "$workspace" commit -q -m "summary root"
        "${gitenv[@]}" git -C "$workspace" branch -M review-base
    fi
    printf 'both staged\nboth base\n' >"$workspace/both.rs"
    printf 'kept base\n' >"$workspace/kept.txt"
    printf 'rename base\n' >"$workspace/old-name.txt"
    printf 'ahead base\n' >"$workspace/ahead.txt"
    printf 'shared base\n' >"$workspace/bare/shared.txt"
    printf 'clean base\n' >"$workspace/clean/clean.txt"
    python3 -c 'import pathlib,sys; pathlib.Path(sys.argv[1]).write_bytes(bytes([0,1,2,3]))' "$workspace/pixel.png"
    "${gitenv[@]}" git -C "$workspace" add -- both.rs kept.txt old-name.txt ahead.txt pixel.png
    "${gitenv[@]}" git -C "$workspace" commit -q -m "summary base"
    origin="$parent/origin.git"
    "${gitenv[@]}" git init -q --bare "$origin"
    "${gitenv[@]}" git -C "$workspace" config protocol.file.allow always
    "${gitenv[@]}" git -C "$workspace" remote add origin "$origin"
    "${gitenv[@]}" git -C "$workspace" push -q origin review-base
    printf 'ahead only\n' >>"$workspace/ahead.txt"
    python3 -c 'import pathlib,sys; pathlib.Path(sys.argv[1]).write_bytes(bytes([0,1,2,255]))' "$workspace/pixel.png"
    "${gitenv[@]}" git -C "$workspace" add -- ahead.txt pixel.png
    "${gitenv[@]}" git -C "$workspace" commit -q -m "summary ahead"
    printf 'both staged\nboth index\n' >"$workspace/both.rs"
    "${gitenv[@]}" git -C "$workspace" add -- both.rs
    printf 'both staged\nboth work\nboth extra\n' >"$workspace/both.rs"
    printf 'added work\n' >"$workspace/added.txt"
    "${gitenv[@]}" git -C "$workspace" add -- added.txt
    "${gitenv[@]}" git -C "$workspace" rm -q -- kept.txt
    "${gitenv[@]}" git -C "$workspace" mv -- old-name.txt new-name.txt
    python3 -c 'import pathlib,sys; pathlib.Path(sys.argv[1]).write_bytes(bytes([9,8,7,6]))' "$workspace/pixel.png"
    "${gitenv[@]}" git -C "$workspace" add -- pixel.png
    printf 'loose work\n' >"$workspace/loose.txt"
    "${gitenv[@]}" git -C "$workspace/bare" init -q
    "${gitenv[@]}" git -C "$workspace/bare" config user.email tui-e2e@example.com
    "${gitenv[@]}" git -C "$workspace/bare" config user.name "TUI E2E"
    "${gitenv[@]}" git -C "$workspace/bare" config commit.gpgsign false
    "${gitenv[@]}" git -C "$workspace/bare" add -- shared.txt
    "${gitenv[@]}" git -C "$workspace/bare" commit -q -m "bare base"
    "${gitenv[@]}" git -C "$workspace/clean" init -q
    "${gitenv[@]}" git -C "$workspace/clean" config user.email tui-e2e@example.com
    "${gitenv[@]}" git -C "$workspace/clean" config user.name "TUI E2E"
    "${gitenv[@]}" git -C "$workspace/clean" config commit.gpgsign false
    "${gitenv[@]}" git -C "$workspace/clean" config protocol.file.allow always
    "${gitenv[@]}" git -C "$workspace/clean" add -- clean.txt
    "${gitenv[@]}" git -C "$workspace/clean" commit -q -m "clean base"
    "${gitenv[@]}" git -C "$workspace/clean" branch -M review-base
    "${gitenv[@]}" git init -q --bare "$parent/clean-origin.git"
    "${gitenv[@]}" git -C "$workspace/clean" remote add origin "$parent/clean-origin.git"
    "${gitenv[@]}" git -C "$workspace/clean" push -q origin review-base
    # Two individually legal unstaged patches whose combined summary budget
    # exceeds 2 MiB. The nested repository is discovered by summary collection
    # but excluded from the review snapshot, so the review fixture counts stay
    # untouched. Expected per-file digests come from real git output normalized
    # exactly like the collector's hunk bodies, never from the mock.
    mkdir -p "$big"
    "${gitenv[@]}" git -C "$big" init -q
    "${gitenv[@]}" git -C "$big" config user.email tui-e2e@example.com
    "${gitenv[@]}" git -C "$big" config user.name "TUI E2E"
    "${gitenv[@]}" git -C "$big" config commit.gpgsign false
    printf 'base\n' >"$big/left.txt"
    printf 'base\n' >"$big/right.txt"
    "${gitenv[@]}" git -C "$big" add -- left.txt right.txt
    "${gitenv[@]}" git -C "$big" commit -q -m "big base"
    python3 - "$big/left.txt" "$big/right.txt" <<'PY'
import pathlib
import sys
LINES = 18_500
def body(tag):
    rows = []
    for index in range(LINES):
        if index == 0:
            row = f"{tag}-HEAD-MARKER"
        elif index == LINES // 2:
            row = f"{tag}-MIDDLE-MARKER"
        elif index == LINES - 1:
            row = f"{tag}-TAIL-MARKER"
        else:
            row = f"{tag}-{index:05d} " + "x" * 48
        rows.append(row + "\n")
    return "".join(rows)
pathlib.Path(sys.argv[1]).write_text(body("LEFT"), encoding="utf-8")
pathlib.Path(sys.argv[2]).write_text(body("RIGHT"), encoding="utf-8")
PY
    "${gitenv[@]}" git -C "$big" --literal-pathspecs diff --no-ext-diff \
        --no-textconv --no-color --find-renames -- left.txt >"$parent/big-left.diff"
    "${gitenv[@]}" git -C "$big" --literal-pathspecs diff --no-ext-diff \
        --no-textconv --no-color --find-renames -- right.txt >"$parent/big-right.diff"
    python3 - "$parent/big-left.diff" "$parent/big-right.diff" \
        "$parent/summary-big-expected.json" <<'PY'
import hashlib
import json
import pathlib
import sys
def patch_body(path):
    lines = pathlib.Path(path).read_text(encoding="utf-8").splitlines(keepends=True)
    starts = [index for index, line in enumerate(lines) if line.startswith("@@ ")]
    if not starts:
        raise SystemExit(f"{path}: diff has no hunk header")
    if any(line.startswith("\\ No newline") for line in lines):
        raise SystemExit(f"{path}: unexpected no-newline marker")
    body = "".join(lines[starts[0]:])
    return body[:-1] if body.endswith("\n") else body
def expected(path, tag):
    body = patch_body(path)
    markers = [f"+{tag}-{part}-MARKER" for part in ("HEAD", "MIDDLE", "TAIL")]
    at = [body.find(marker) for marker in markers]
    if min(at) < 0 or at != sorted(at) or len(set(at)) != 3:
        raise SystemExit(f"{path}: head/middle/tail markers missing or out of order")
    if not body.endswith(markers[2]):
        raise SystemExit(f"{path}: patch does not end at the tail marker")
    data = body.encode("utf-8")
    if not 0 < len(data) < 2 * 1024 * 1024:
        raise SystemExit(f"{path}: patch body is {len(data)} bytes")
    return {"token": tag + "TOK", "sha256": hashlib.sha256(data).hexdigest(),
            "bytes": len(data), "markers": [marker[1:] for marker in markers]}
patches = {"left.txt": expected(sys.argv[1], "LEFT"),
           "right.txt": expected(sys.argv[2], "RIGHT")}
total = sum(item["bytes"] for item in patches.values())
if total <= 2 * 1024 * 1024:
    raise SystemExit(f"summary patches total {total} bytes, not above the 2 MiB cap")
pathlib.Path(sys.argv[3]).write_text(
    json.dumps({"patches": patches, "total_bytes": total}, indent=2) + "\n",
    encoding="utf-8")
PY
    [ -s "$parent/summary-big-expected.json" ] || fail "fixture: large summary expectation missing"
    ahead="$("${gitenv[@]}" git -C "$workspace" rev-list --count origin/review-base..HEAD)"
    [ "$ahead" = 1 ] || fail "fixture: primary is not exactly one commit ahead (${ahead:-empty})"
    head_oid="$("${gitenv[@]}" git -C "$workspace" rev-parse HEAD)"
    origin_oid="$("${gitenv[@]}" git -C "$workspace" rev-parse origin/review-base)"
    [ "$head_oid" != "$origin_oid" ] || fail "fixture: primary HEAD matches origin"
    case "$("${gitenv[@]}" git -C "$workspace" status --porcelain=v1)" in
        *"MM both.rs"*) ;;
        *) fail "fixture: both.rs is not MM" ;;
    esac
    case "$("${gitenv[@]}" git -C "$workspace" status --porcelain=v1)" in
        *"A  added.txt"*) ;;
        *) fail "fixture: added.txt is not staged added" ;;
    esac
    case "$("${gitenv[@]}" git -C "$workspace" status --porcelain=v1)" in
        *"D  kept.txt"*) ;;
        *) fail "fixture: kept.txt is not staged deleted" ;;
    esac
    case "$("${gitenv[@]}" git -C "$workspace" status --porcelain=v1)" in
        *"R  old-name.txt -> new-name.txt"*) ;;
        *) fail "fixture: rename endpoints missing" ;;
    esac
    case "$("${gitenv[@]}" git -C "$workspace" status --porcelain=v1)" in
        *"M  pixel.png"*) ;;
        *) fail "fixture: pixel.png is not a staged modification" ;;
    esac
    case "$("${gitenv[@]}" git -C "$workspace" status --porcelain=v1)" in
        *"?? loose.txt"*) ;;
        *) fail "fixture: loose.txt is not untracked" ;;
    esac
    [ -z "$("${gitenv[@]}" git -C "$workspace/bare" status --porcelain=v1)" ] || fail "fixture: bare is not clean"
    [ -z "$("${gitenv[@]}" git -C "$workspace/bare" remote)" ] || fail "fixture: bare has an origin"
    [ -z "$("${gitenv[@]}" git -C "$workspace/clean" status --porcelain=v1)" ] || fail "fixture: clean is not clean"
    [ "$("${gitenv[@]}" git -C "$workspace/clean" rev-list --count origin/review-base..HEAD)" = 0 ] || fail "fixture: clean is ahead"
    both_staged="$("${gitenv[@]}" git -C "$workspace" diff --cached --numstat -- both.rs)"
    both_unstaged="$("${gitenv[@]}" git -C "$workspace" diff --numstat -- both.rs)"
    python3 - "$parent/summary-numstat.txt" "$both_staged" "$both_unstaged" <<'PY'
import pathlib, sys
path, staged, unstaged = sys.argv[1:]
def needle(row, label):
    added, deleted, *_rest = row.split()
    if not added.isdigit() or not deleted.isdigit():
        raise SystemExit(f'{label} numstat is not text: {row}')
    return f'M both.rs +{added}/-{deleted}'
staged_needle, unstaged_needle = needle(staged, 'staged'), needle(unstaged, 'unstaged')
if staged_needle == unstaged_needle:
    raise SystemExit('both.rs layers have the same numstat')
pathlib.Path(path).write_text(staged_needle + '\n' + unstaged_needle + '\n', encoding='utf-8')
PY
}

write_ops_mock() { # root
    cat >"$1/ops_mock_server.py" <<'PY'
#!/usr/bin/env python3
"""Strict code-review-ops provider mock: content-routed SSE with release gates."""
import json
import re
import hashlib
import select
import socket
import sys
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

PORT_FILE = sys.argv[1]
EXPECTED_PATH = sys.argv[2] if len(sys.argv) > 2 else ""
EXPECTED = None
LARGE_SEEN = {}
GATE_LOCK = threading.Lock()
GATES = {}
RELEASE_DONE = {}
SERIAL = 0
ZH_TAG = "Respond in the language tagged `zh-CN`."
AUTO_REPLY_ERROR = "ok"
HIGHLIGHT_SEEN = 0
TLDR_LATE = False
SUMMARY_MARKER = "Summarize repository changes using the required purpose JSON.\n"
DESIGNATED_KEYS = "\nDesignated purpose keys JSON:\n"
SUMMARY_FACTS = "\nRepository facts JSON:\n"
STALE_PURPOSE = "STALE-LATE-SNAPSHOT"
DESIGNATED_CONTRACT = "This batch designates exactly these keys."
OLD_ALL_KEYS = "Include every repository, commit, committed file, staged file, unstaged file, and untracked file from the facts exactly once."
KIND_ORDER = ("commit", "committed_file", "staged", "unstaged", "untracked")
TLDR_GENERATION = 0
TLDR_SEEN = 0
PURPOSE_SCHEMA = {
    "type": "object",
    "additionalProperties": False,
    "required": ["repositories", "changes"],
    "properties": {
        "repositories": {"type": "array", "items": {"type": "object", "additionalProperties": False, "required": ["repo", "purpose"], "properties": {"repo": {"type": "string", "minLength": 1}, "purpose": {"type": "string", "minLength": 1}}}},
        "changes": {"type": "array", "items": {"type": "object", "additionalProperties": False, "required": ["repo", "kind", "id", "purpose"], "properties": {"repo": {"type": "string", "minLength": 1}, "kind": {"type": "string", "enum": ["commit", "committed_file", "staged", "unstaged", "untracked"]}, "id": {"type": "string", "minLength": 1}, "purpose": {"type": "string", "minLength": 1}}}},
    },
}

def purpose_for(record):
    patch = record.get("patch") or ""
    lines = record.get("lines") or {}
    stats = f"+{lines.get('insertions', 0)}/-{lines.get('deletions', 0)}" if lines.get("kind") == "text" else lines.get("kind", "unknown")
    previous = f"{record['previous_path']} -> " if record.get("previous_path") else ""
    marker = "work" if "both work" in patch else "index" if "both index" in patch else "ahead" if "ahead only" in patch else ""
    return f"{record.get('status')} {previous}{record.get('path')} {stats} {marker}".strip()

def origin_purpose(repository):
    origin = repository.get("origin") or {}
    outcome = origin.get("outcome", "unknown")
    return f"{repository['label']} {outcome} ahead {origin.get('ahead', 0)}"

def change_purpose(kind, record):
    return record["subject"] if kind == "commit" else purpose_for(record)

def designated_document(facts):
    changes = []
    for repository in facts["repositories"]:
        for commit in repository.get("commits") or []:
            changes.append({"repo": repository["label"], "kind": "commit", "id": commit["oid"]})
            for record in commit.get("files") or []:
                changes.append({"repo": repository["label"], "kind": "committed_file", "id": f"{commit['oid']}:{record['path']}"})
        for kind in ("staged", "unstaged", "untracked"):
            for record in repository.get(kind) or []:
                changes.append({"repo": repository["label"], "kind": kind, "id": record["path"]})
    changes.sort(key=lambda item: (item["repo"], KIND_ORDER.index(item["kind"]), item["id"]))
    return {"repositories": [repository["label"] for repository in facts["repositories"]], "changes": changes}

def locate_change(repository, item):
    if item["kind"] == "commit":
        return next((record for record in repository.get("commits") or [] if record["oid"] == item["id"]), None)
    if item["kind"] == "committed_file":
        oid, path = item["id"].split(":", 1)
        for commit in repository.get("commits") or []:
            if commit["oid"] != oid:
                continue
            return next((record for record in commit.get("files") or [] if record["path"] == path), None)
        return None
    return next((record for record in repository.get(item["kind"]) or [] if record["path"] == item["id"]), None)

def expected_patches():
    global EXPECTED
    if EXPECTED is None:
        if not EXPECTED_PATH:
            raise ValueError("large summary expectation path is not configured")
        with open(EXPECTED_PATH, encoding="utf-8") as handle:
            EXPECTED = json.load(handle)["patches"]
    return EXPECTED

def verify_large(record):
    patches = expected_patches()
    expected = patches.get(record.get("path"))
    if expected is None:
        return None
    patch = record.get("patch") or ""
    data = patch.encode("utf-8")
    digest = hashlib.sha256(data).hexdigest()
    if len(data) != expected["bytes"] or digest != expected["sha256"]:
        raise ValueError(f"large patch bytes/digest mismatch path={record.get('path')}")
    at = [patch.find("+" + marker) for marker in expected["markers"]]
    if min(at) < 0 or at != sorted(at) or len(set(at)) != 3:
        raise ValueError(f"large patch head/middle/tail order mismatch path={record.get('path')}")
    if not patch.endswith("+" + expected["markers"][2]):
        raise ValueError(f"large patch tail mismatch path={record.get('path')}")
    with GATE_LOCK:
        if record["path"] in LARGE_SEEN:
            raise ValueError(f"large patch duplicated across batches path={record.get('path')}")
        LARGE_SEEN[record["path"]] = digest
        complete = len(LARGE_SEEN) == len(patches)
        log(f"tldr-large file={record['path']} sha256={digest} bytes={len(data)} head=1 middle=1 tail=1")
        if complete:
            log(f"tldr-large coverage=complete files={len(LARGE_SEEN)}")
    return expected["token"]

def purpose_document(text, purpose_override=None):
    if not text.startswith(SUMMARY_MARKER) or DESIGNATED_KEYS not in text or SUMMARY_FACTS not in text:
        raise ValueError("summary prompt shape")
    keys_at = text.index(DESIGNATED_KEYS)
    facts_at = text.index(SUMMARY_FACTS)
    if not (len(SUMMARY_MARKER) < keys_at < facts_at):
        raise ValueError("summary section order")
    contract = text[len(SUMMARY_MARKER):keys_at]
    if OLD_ALL_KEYS in contract:
        raise ValueError("old all-keys protocol")
    language_at = contract.rfind("Write every purpose in the language tagged `")
    if language_at >= 0 and ("\n" in contract[language_at:] or not contract[language_at:].endswith(".")):
        raise ValueError("language line is not the last contract line before Designated purpose keys")

    designated = json.loads(text[keys_at + len(DESIGNATED_KEYS):facts_at].strip())
    facts = json.loads(text[facts_at + len(SUMMARY_FACTS):].strip())
    if not isinstance(designated.get("repositories"), list) or not isinstance(designated.get("changes"), list):
        raise ValueError("designated keys shape")
    if any(not isinstance(label, str) or not label for label in designated["repositories"]):
        raise ValueError("designated repositories are not labels")
    for item in designated["changes"]:
        if set(item) != {"repo", "kind", "id"} or not all(
            isinstance(item[key], str) and item[key] for key in ("repo", "kind", "id")
        ):
            raise ValueError("designated change is not repo kind id")
    labels = {repository["label"]: repository for repository in facts["repositories"]}
    if len(labels) != 1 or len(facts["repositories"]) != 1:
        raise ValueError("summary facts are not one repository")
    full = designated_document(facts)
    if designated["repositories"] not in ([], full["repositories"]):
        raise ValueError("designated repository key is neither absent nor the repository")
    designated_keys = [tuple(item[key] for key in ("repo", "kind", "id")) for item in designated["changes"]]
    full_keys = [tuple(item[key] for key in ("repo", "kind", "id")) for item in full["changes"]]
    if len(designated_keys) != len(set(designated_keys)) or sorted(designated_keys) != sorted(full_keys):
        raise ValueError("designated changes are not exactly the facts keys")
    large_tokens = {}
    for repository in facts["repositories"]:
        for record in repository.get("unstaged") or []:
            token = verify_large(record)
            if token is not None:
                large_tokens[record["path"]] = token
    full_ids = {(item["repo"], item["kind"], item["id"]) for item in full["changes"]}
    seen = set()
    purposes = []
    changes = []
    def purpose(value):
        return purpose_override or value
    for label in designated["repositories"]:
        if label not in labels:
            raise ValueError(f"designated repository missing from facts {label}")
        purposes.append({"repo": label, "purpose": purpose(origin_purpose(labels[label]))})
    for item in designated["changes"]:
        if set(item) != {"repo", "kind", "id"}:
            raise ValueError("designated change is not repo kind id")
        identity = (item["repo"], item["kind"], item["id"])
        if identity not in full_ids or identity in seen:
            raise ValueError(f"designated change is not an owned fact key {item['kind']} {item['id']}")
        seen.add(identity)
        matched = locate_change(labels[item["repo"]], item)
        if matched is None:
            raise ValueError(f"designated key missing from facts {item['kind']} {item['id']}")
        value = change_purpose(item["kind"], matched)
        token = large_tokens.get(matched.get("path"))
        if token:
            value = f"{value} {token}"
        changes.append({"repo": item["repo"], "kind": item["kind"], "id": item["id"], "purpose": purpose(value)})
    return json.dumps({"repositories": purposes, "changes": changes}, separators=(",", ":"))
MANUAL_REPLY = "MANUAL-REPLY-OPS-BODY 人工复核：改动合理。\n"
COMMENT_SENTINEL = "OPS-COMMENT-SENTINEL-01"
IDENTITY_KEYS = ("snapshotId", "path", "oldStart", "oldCount", "newStart", "newCount", "contentHash")



def log(line):
    print(f"ops-mock {line}", file=sys.stderr, flush=True)


def register_gate(kind):
    gate = threading.Event()
    record = threading.Event()
    record.delivered = False
    record.outcome = None
    with GATE_LOCK:
        if kind == "tldr":
            GATES.setdefault(kind, []).append((gate, record))
        else:
            GATES[kind] = gate
            RELEASE_DONE[kind] = record
    return gate, record


def pop_gate(kind):
    with GATE_LOCK:
        queued = GATES.get(kind)
        if isinstance(queued, list):
            item = queued.pop(0) if queued else None
            gate = item[0] if item is not None else None
            if not queued:
                GATES.pop(kind, None)
        else:
            gate = GATES.pop(kind, None)
    if gate is not None:
        gate.set()
    return gate is not None

def begin_release(kind):
    with GATE_LOCK:
        queued = GATES.get(kind)
        if isinstance(queued, list):
            if not queued:
                return None
            gate, record = queued.pop(0)
            if not queued:
                GATES.pop(kind, None)
        else:
            gate = GATES.pop(kind, None)
            record = RELEASE_DONE.get(kind)
            if gate is None or record is None:
                return None
        gate.set()
    return record

def finish_release(record, delivered, outcome):
    if record is not None and not record.is_set():
        record.delivered = delivered
        record.outcome = outcome
        record.set()

def note_tldr_request():
    global TLDR_GENERATION, TLDR_SEEN
    with GATE_LOCK:
        if TLDR_GENERATION == 0:
            TLDR_GENERATION = 1
            TLDR_SEEN = 0
        TLDR_SEEN += 1
        generation = TLDR_GENERATION
        seen = TLDR_SEEN
    log(f"tldr generation={generation} batch={seen}")

def advance_tldr_generation():
    global TLDR_GENERATION, TLDR_SEEN, TLDR_LATE
    with GATE_LOCK:
        TLDR_GENERATION += 1
        TLDR_SEEN = 0
        if TLDR_LATE != TLDR_GENERATION:
            TLDR_LATE = False

def arm_tldr_late():
    global TLDR_LATE
    with GATE_LOCK:
        TLDR_LATE = TLDR_GENERATION or 1

def take_tldr_late(generation):
    global TLDR_LATE
    with GATE_LOCK:
        late = TLDR_LATE == generation
    return late


def sse(payload):
    return f"data: {json.dumps(payload, separators=(',', ':'))}\n\n"


def delta(tag, text, finish=None, model=None):
    choice = {"index": 0, "delta": {"content": text}}
    if finish is not None:
        choice["finish_reason"] = finish
    payload = {"id": tag, "choices": [choice]}
    if model is not None:
        payload["model"] = model
    return payload


def stream_text(handler, tag, text, model=None):
    body = sse(delta(tag, "", model=model)) + sse(delta(tag, text, finish="stop", model=model))
    handler.send_response(200)
    handler.send_header("content-type", "text/event-stream")
    handler.send_header("content-length", str(len(body)))
    handler.end_headers()
    handler.wfile.write(body.encode())
    handler.wfile.flush()


def peer_closed(handler):
    ready, _, _ = select.select([handler.connection], [], [], 0)
    if not ready:
        return False
    try:
        return handler.connection.recv(1, socket.MSG_PEEK) == b""
    except (BrokenPipeError, ConnectionResetError):
        return True


def stream_held(handler, kind, final_text, model=None):
    gate, record = register_gate(kind)
    log(f"{kind} gate=entered")
    tag = f"ops-{kind}"
    handler.send_response(200)
    handler.send_header("content-type", "text/event-stream")
    handler.end_headers()
    try:
        handler.wfile.write(sse(delta(tag, "", model=model)).encode())
        handler.wfile.flush()
        for _ in range(1800):
            if peer_closed(handler):
                log(f"{kind} peer-closed before release")
                finish_release(record, False, "peer_closed")
                return
            if gate.wait(timeout=0.25):
                break
        else:
            log(f"{kind} hold expired")
            finish_release(record, False, "expired")
            return
        delivered = False
        try:
            if not peer_closed(handler):
                handler.wfile.write(sse(delta(tag, final_text, finish="stop", model=model)).encode())
                handler.wfile.write(b"data: [DONE]\n\n")
                handler.wfile.flush()
                delivered = True
        except (BrokenPipeError, ConnectionResetError):
            delivered = False
        log(f"{kind} released")
        finish_release(record, delivered, "delivered" if delivered else "peer_closed")
    except (BrokenPipeError, ConnectionResetError):
        log(f"{kind} aborted by client")
        finish_release(record, False, "peer_closed")
    except OSError as error:
        log(f"{kind} unexpected socket error={type(error).__name__}: {error}")
        finish_release(record, False, "unexpected_error")
        raise

def message_text(message):
    content = message.get("content")
    if isinstance(content, str):
        return content
    if isinstance(content, list):
        chunks = []
        for block in content:
            if isinstance(block, dict) and isinstance(block.get("text"), str):
                chunks.append(block["text"])
        if chunks:
            return "\n".join(chunks)
    return None


def full_text(body):
    chunks = []
    for message in body.get("messages", []):
        text = message_text(message)
        if text is not None:
            chunks.append(text)
    return "\n".join(chunks)


def last_user_text(body):
    for message in reversed(body.get("messages", [])):
        if message.get("role") == "user":
            text = message_text(message)
            return "" if text is None else text
    return ""


def prompt_identities(prompt):
    identities = []
    for line in prompt.splitlines():
        if not line.startswith("Hunk identity: "):
            continue
        fields = {}
        for field in line[len("Hunk identity: "):].split():
            key, separator, value = field.partition("=")
            if separator != "=":
                raise ValueError("identity field")
            fields[key] = value
        if tuple(fields) != IDENTITY_KEYS:
            raise ValueError("identity keys")
        for key in ("oldStart", "oldCount", "newStart", "newCount"):
            fields[key] = int(fields[key])
        if len(fields["contentHash"]) != 64:
            raise ValueError("content hash")
        identities.append(fields)
    return identities


def numbered_rows(prompt, identity):
    marker = (
        "Hunk identity: snapshotId={snapshotId} path={path} oldStart={oldStart} "
        "oldCount={oldCount} newStart={newStart} newCount={newCount} contentHash={contentHash}"
    ).format(**identity)
    start = prompt.index(marker)
    rows = []
    for line in prompt[start + len(marker):].splitlines()[1:]:
        if line.startswith("Hunk identity: ") or (line.startswith("File: ") and rows):
            break
        match = re.match(r"^\s*(\d+|-)\s+(\d+|-)\s+([ +\-@])(.*)$", line)
        if match:
            rows.append({
                "old": None if match.group(1) == "-" else int(match.group(1)),
                "new": None if match.group(2) == "-" else int(match.group(2)),
                "marker": match.group(3),
                "text": match.group(4),
            })
    return rows


def one_finding(category, side, line, summary, evidence):
    return {
        "category": category,
        "side": side,
        "startLine": line,
        "endLine": line,
        "summary": summary,
        "evidence": evidence,
    }


def findings_for(identity, rows):
    text = "\n".join(row["text"] for row in rows)
    if "DEEP-HUNK-MARK-0240" in text:
        deep = next(row["new"] for row in rows if row["marker"] == "+" and "DEEP-HUNK-MARK-0240" in row["text"])
        second = next(row["new"] for row in rows if row["marker"] == "+" and row["new"] != deep)
        return [
            one_finding("critical", "new", deep, "Critical: deep marker has no guard", "DEEP-HUNK-MARK-0240"),
            one_finding("naming", "new", second, "Naming: second deep row", next(row["text"].strip() for row in rows if row["new"] == second)),
        ]
    if "OPS-PADMIDDLE" in text:
        line = next(row["new"] for row in rows if row["marker"] == "+" and "OPS-PADMIDDLE" in row["text"])
        return [one_finding("overdesign", "new", line, "Overdesign: middle pad duplicates state", "OPS-PADMIDDLE")]
    if "EARLY-HUNK-FLAG-0012" in text:
        line = next(row["old"] for row in rows if row["marker"] == "-" and row["old"] is not None)
        evidence = next(row["text"].strip() for row in rows if row["old"] == line and row["marker"] == "-")
        return [one_finding("naming", "old", line, "Naming: deleted early baseline", evidence)]
    return []


def findings_document(prompt, require_complete):
    identities = prompt_identities(prompt)
    if require_complete and len(identities) < 2:
        raise ValueError("highlight scope")
    hunks = []
    for identity in identities:
        hunks.append({"identity": identity, "findings": findings_for(identity, numbered_rows(prompt, identity))})
    if require_complete:
        nonempty = [hunk for hunk in hunks if hunk["findings"]]
        if len(nonempty) != 3:
            raise ValueError("highlight findings")
    return {"hunks": hunks}



TRIAGE_SCHEMA = {
    "type": "object",
    "additionalProperties": False,
    "required": ["core", "skipped"],
    "properties": {
        "core": {"type": "array", "items": {"type": "string"}},
        "skipped": {"type": "integer", "minimum": 0},
    },
}


def assert_triage_schema(body):
    response_format = body.get("response_format") or {}
    schema = response_format.get("json_schema") or {}
    return (
        response_format.get("type") == "json_schema"
        and schema.get("name") == "review_triage"
        and schema.get("strict") is True
        and schema.get("schema") == TRIAGE_SCHEMA
        and "TRIAGE:" not in full_text(body)
    )


def triage_selection(text):
    """Strict review_triage body: one path@oldStart-newStart token for the deep hunk."""
    if "TRIAGE:" in text:
        raise ValueError("obsolete trailer")
    tokens = re.findall(r"^Hunk: (\S+)$", text, re.MULTILINE)
    if len(tokens) < 2 or len(tokens) != len(set(tokens)):
        raise ValueError("triage hunks")
    core = [token for token in tokens if "DEEP-HUNK-MARK-0240" in text.split(f"Hunk: {token}", 1)[1].split("\nHunk: ", 1)[0]]
    if len(core) != 1 or not re.fullmatch(r"\S+@\d+-\d+", core[0]):
        raise ValueError("deep token")
    return {"core": core, "skipped": len(tokens) - 1}

def schema_name(body):
    return ((body.get("response_format") or {}).get("json_schema") or {}).get("name")


def assert_findings_schema(body):
    response_format = body.get("response_format") or {}
    schema = (response_format.get("json_schema") or {})
    contract = response_format.get("type") == "json_schema" and schema.get("name") == "review_findings" and schema.get("strict") is True
    required = ((schema.get("schema") or {}).get("properties") or {}).get("hunks", {}).get("items", {}).get("properties", {}).get("identity", {}).get("required")
    return contract and required == list(IDENTITY_KEYS)


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.0"

    def do_GET(self):
        self.send_response(404)
        self.end_headers()

    def do_POST(self):
        try:
            length = int(self.headers.get("content-length", "0"))
            raw = self.rfile.read(length)
            if self.path == "/__tldr-generation":
                advance_tldr_generation()
                log("tldr-generation=advanced")
                body = b'{"generation": true}\n'
                self.send_response(200)
                self.send_header("content-type", "application/json")
                self.send_header("content-length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)
                return
            if self.path == "/__tldr-late":
                arm_tldr_late()
                log("tldr-late=armed")
                body = b'{"late": true}\n'
                self.send_response(200)
                self.send_header("content-type", "application/json")
                self.send_header("content-length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)
                return
            if self.path.startswith("/__release-"):
                gate_map = {"triage": "triage", "highlight": "highlight", "auto": "auto", "tldr": "tldr"}
                kind = gate_map[self.path.rsplit("-", 1)[-1]]
                done = begin_release(kind)
                delivered = False
                outcome = None
                if done is not None:
                    done.wait(5)
                    delivered = done.delivered
                    outcome = done.outcome
                held = done is not None
                log(f"release={kind} held={str(held).lower()} delivered={str(delivered).lower()} outcome={outcome}")
                body = ('{"released":%s,"delivered":%s,"outcome":%s}\n' % ("true" if held else "false", "true" if delivered else "false", json.dumps(outcome))).encode()
                self.send_response(200)
                self.send_header("content-type", "application/json")
                self.send_header("content-length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)
                return
            body = json.loads(raw)
            text = full_text(body)
            current = last_user_text(body)
            if COMMENT_SENTINEL in text and "User comment:" in text:
                kind = "manual"
            elif "Classify every hunk" in text:
                kind = "triage"
            elif "Requested hunks:" in text and "Hunk identity:" in text:
                identities = prompt_identities(text)
                kind = "auto" if len(identities) == 1 else "highlight"
            elif current.startswith(SUMMARY_MARKER) and SUMMARY_FACTS in current:
                kind = "tldr"
            else:
                kind = "unrouted"
            with GATE_LOCK:
                type(self).bump_serial()
            log(f"request#{SERIAL} kind={kind} prompt_len={len(text)}")
            self.route(kind, body, current if kind == "tldr" else text)
        except (BrokenPipeError, ConnectionResetError):
            log("client aborted mid-request")
        except Exception as error:
            log(f"request error={type(error).__name__}: {error}")
            try:
                stream_text(self, "ops-error", "mock rejected")
            except OSError:
                pass

    @classmethod
    def bump_serial(cls):
        global SERIAL
        SERIAL += 1

    def route(self, kind, body, text):
        tag = f"ops-{kind}"
        if kind == "triage":
            if not assert_triage_schema(body):
                log("triage strict=missing")
                stream_text(self, tag, '{"error": "mock requires strict review_triage schema"}')
                return
            selection = triage_selection(text)
            log("triage strict=ok")
            log(f"triage token={selection['core'][0]} skipped={selection['skipped']}")
            stream_held(self, kind, json.dumps(selection, separators=(',', ':')))
        elif kind == "auto":
            if ZH_TAG not in text:
                log("auto lang=missing")
                stream_text(self, tag, '{"error": "zh-CN review language tag missing"}')
                return
            if schema_name(body) not in (None, "review_findings"):
                log("auto-schema=unexpected")
                stream_text(self, tag, '{"error": "unexpected auto schema"}')
                return
            identities = prompt_identities(text)
            log("auto-schema=absent" if schema_name(body) is None else "auto-schema=review_findings")
            log("auto lang=zh-CN")
            log(f"auto-scope={len(identities)}")
            if AUTO_REPLY_ERROR == "range":
                document = findings_document(text, False)
                document["hunks"][0]["findings"][0]["endLine"] = identities[0]["newStart"] + identities[0]["newCount"] + 40
                stream_held(self, kind, json.dumps(document), model="ops-switch-model")
                return
            stream_held(self, kind, json.dumps(findings_document(text, False)), model="ops-switch-model")
        elif kind == "highlight":
            if not assert_findings_schema(body):
                log("findings-schema=missing")
                stream_text(self, tag, '{"error": "review_findings schema missing"}')
                return
            if ZH_TAG not in text:
                log("highlight lang=missing")
                stream_text(self, tag, '{"error": "zh-CN review language tag missing"}')
                return
            identities = prompt_identities(text)
            log("findings-schema=ok")
            log("highlight lang=zh-CN")
            log("highlight-scope=complete" if len(identities) >= 2 else "highlight-scope=short")
            document = findings_document(text, True)
            global HIGHLIGHT_SEEN
            HIGHLIGHT_SEEN += 1
            if HIGHLIGHT_SEEN >= 4:
                log("highlight-invalid-range")
                target = next(hunk for hunk in document["hunks"] if hunk["findings"])
                target["findings"][0]["endLine"] = target["identity"]["newCount"] + 80
                target["findings"][0]["summary"] = "STALE-RANGE"
            stream_held(self, kind, json.dumps(document))
        elif kind == "tldr":
            note_tldr_request()
            prompt_data = text.encode("utf-8")
            log(f"tldr prompt_bytes={len(prompt_data)} prompt_digest={hashlib.sha256(prompt_data).hexdigest()}")
            response_format = body.get("response_format") or {}
            schema = response_format.get("json_schema") or {}
            purpose_schema = response_format.get("type") == "json_schema" and schema.get("name") == "review_purpose" and schema.get("strict") is True and schema.get("schema") == PURPOSE_SCHEMA
            log("tldr-schema=review_purpose" if purpose_schema else "tldr-schema=rejected")
            log("tldr-tools=absent" if not body.get("tools") else "tldr-tools=present")
            if OLD_ALL_KEYS in text or "Write one Markdown document" in text:
                log("tldr contract=old-protocol")
            else:
                log("tldr contract=old-protocol-absent")
            if not purpose_schema or body.get("tools") or "comparisonLabel:" in text or "Write one Markdown document" in text or OLD_ALL_KEYS in text or DESIGNATED_KEYS not in text:
                log("tldr contract=rejected")
                stream_text(self, tag, "TLDR contract missing")
                return
            log("tldr contract=purpose-json")
            if take_tldr_late(TLDR_GENERATION):
                log("tldr-late=consumed")
                stream_held(self, kind, purpose_document(text, STALE_PURPOSE))
            else:
                stream_held(self, kind, purpose_document(text))
        elif kind == "manual":
            stream_text(self, tag, MANUAL_REPLY, model="ops-switch-model")
        else:
            stream_text(self, tag, '{"error": "mock unrouted"}')


def main():
    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    with open(PORT_FILE, "w", encoding="utf-8") as handle:
        handle.write(str(server.server_port))
    log(f"listening port={server.server_port}")
    server.serve_forever()


if __name__ == "__main__":
    main()
PY
}

start_ops_mock() { # root [expected-patches]
    local root="$1" expected="${2:-}" port_file="$root/mock-port.txt" deadline port
    python3 "$root/ops_mock_server.py" "$port_file" "$expected" >"$evidence/mock-server.log" 2>&1 &
    register_pid $!
    deadline=$((SECONDS + 15))
    while [ ! -s "$port_file" ] && [ "$SECONDS" -lt "$deadline" ]; do sleep 0.2; done
    [ -s "$port_file" ] || fail "user.$SCENARIO: ops mock did not write its port file"
    port="$(cat "$port_file")"
    printf '%s\n' "$port"
}

main() {
    require_rpi
    require_cmd tmux
    require_cmd git
    require_cmd python3
    require_cmd curl

    local root evidence session port size
    root="$(scenario_workspace "$SCENARIO")"
    evidence="$EVIDENCE_ROOT/$SCENARIO"
    session="$(unique_tmux_name "$SCENARIO")"
    register_tmux_session "$session"
    seed_review_fixture "$root"
    write_ops_mock "$root"
    port="$(start_ops_mock "$root" "$root/summary/summary-big-expected.json")"
    mock_port="$port"

    mkdir -p "$root/home/.pi/agent"
    for settings in "$root/home/.pi/agent/settings.json" "$root/home/.rpi/settings.json"; do
        printf '{\n  "reviewLanguage": "zh-CN"\n}\n' >"$settings"
    done
    cat >"$root/home/.pi/agent/models.json" <<EOF
{
  "providers": {
    "$PROVIDER": {
      "baseUrl": "http://127.0.0.1:$port",
      "api": "openai-completions",
      "models": [
        {
          "id": "$MODEL_ID",
          "name": "Ops Review Mock",
          "contextWindow": 1000000,
          "maxTokens": 8192,
          "compat": { "supportsResponseSchema": true }
        }
      ]
    }
  }
}
EOF

    rpi_pane_command() { # done-sentinel
        local done="$1" stderr="2>>$(printf '%q' "$evidence/rpi-stderr.log")"
        printf '%s' "env HOME=$(printf '%q' "$root/home") USERPROFILE=$(printf '%q' "$root/home") PI_CODING_AGENT_DIR=$(printf '%q' "$root/home/.pi/agent") RPI_OFFLINE=1 RPI_SKIP_VERSION_CHECK=1 TERM=xterm-256color $(printf '%q' "$RPI_BIN") --offline --model $(printf '%q' "$PROVIDER/$MODEL_ID") --api-key ops-mock-key ${stderr}; printf '%s\n' $(printf '%q' "$done")"
    }

    # tmux 3.4 default window-size=latest ignores new-session -x/-y for the
    # live window (this server: latest, observed 40x17 from the newest client;
    # -x/-y only sets session default-size). resize-window pins this window to
    # manual 140x42 before rpi reads the pty. Global window-size stays latest.
    tmux new-session -d -s "$session" -x 140 -y 42 -c "$root/workspace" \
        'sleep 86400' \
        \; set-option -t "$session" remain-on-exit on \
        \; resize-window -t "$session":0 -x 140 -y 42
    size="$(tmux display-message -p -t "$session":0 '#{window_width}x#{window_height}')"
    [ "$size" = "140x42" ] || fail "user.$SCENARIO: pane is ${size}, expected 140x42"
    tmux respawn-pane -k -c "$root/workspace" -t "$session":0 \
        "$(rpi_pane_command '===TUI-DONE-code-review-operations===')"

    wait_for "$PROVIDER/$MODEL_ID" 30
    capture_to "$evidence/00-boot.txt"
    tmux send-keys -t "$session":0 Escape
    sleep 0.3

    tmux send-keys -t "$session":0 -l '/review review-base'
    wait_for '/review review-base' 10
    tmux send-keys -t "$session":0 Enter
    wait_all 45 'Code review · review-base → working tree · 8 files' \
        'focus:tree · A Auto · H Highlight' \
        'lib.rs' 'z_big.md' '@@' "$EARLY_MARKER" \
        'added.txt' 'both.rs' 'kept.txt' 'loose.txt' 'new-name.txt' 'pixel.png'
    capture_to "$evidence/01-open.txt"
    assert_file_contains "$evidence/01-open.txt" \
        'Code review · review-base → working tree · 8 files' \
        'focus:tree · A Auto · H Highlight' \
        'lib.rs' 'z_big.md' '@@' "$EARLY_MARKER" \
        'added.txt' 'both.rs' 'kept.txt' 'loose.txt' 'new-name.txt' 'pixel.png'
    assert_file_lacks "$evidence/01-open.txt" "$DEEP_MARKER" '· on demand' '■ Stop'

    tmux send-keys -t "$session":0 Left
    wait_for 'Select a file' 10
    tmux send-keys -t "$session":0 Left
    wait_gone 'lib.rs' 10 fold-collapse
    capture_to "$evidence/02-folded.txt"
    assert_file_contains "$evidence/02-folded.txt" 'z_big.md'
    assert_file_lacks "$evidence/02-folded.txt" 'lib.rs' "$EARLY_MARKER"

    tmux send-keys -t "$session":0 Right
    wait_for 'lib.rs' 10
    capture_to "$evidence/03-expanded.txt"
    assert_file_contains "$evidence/03-expanded.txt" 'lib.rs' 'z_big.md'
    assert_file_lacks "$evidence/03-expanded.txt" "$EARLY_MARKER"

    tmux send-keys -t "$session":0 Down
    wait_for "$EARLY_MARKER" 10
    capture_to "$evidence/03b-refocused.txt"
    assert_file_contains "$evidence/03b-refocused.txt" 'lib.rs' "$EARLY_MARKER" 'z_big.md'

    for _ in 1 2 3 4 5 6 7; do
        tmux send-keys -t "$session":0 Down
    done
    wait_for '· on demand' 10
    capture_to "$evidence/04-on-demand.txt"
    assert_file_contains "$evidence/04-on-demand.txt" 'z_big.md' '· on demand'
    assert_file_lacks "$evidence/04-on-demand.txt" "$DEEP_MARKER"

    for _ in 1 2 3 4 5 6 7; do
        tmux send-keys -t "$session":0 Up
    done
    wait_for "$EARLY_MARKER" 10
    capture_to "$evidence/05-before-jump.txt"
    assert_file_contains "$evidence/05-before-jump.txt" 'lib.rs' "$EARLY_MARKER"
    assert_file_lacks "$evidence/05-before-jump.txt" "$DEEP_MARKER"

    tmux send-keys -t "$session":0 ']'
    tmux send-keys -t "$session":0 ']'
    wait_all 15 'focus:diff' "$DEEP_HEADER"
    capture_to "$evidence/06-deep-jump-header.txt"
    assert_file_contains "$evidence/06-deep-jump-header.txt" 'focus:diff' "$DEEP_HEADER"
    tmux send-keys -t "$session":0 PageDown
    wait_all 15 'focus:diff' "$DEEP_MARKER"
    capture_to "$evidence/06-deep-jump.txt"
    assert_file_contains "$evidence/06-deep-jump.txt" 'focus:diff' "$DEEP_MARKER"

    tmux send-keys -t "$session":0 a
    wait_for 'Auto Review triaging ■ Stop' 20
    capture_to "$evidence/07-triage-held.txt"
    assert_file_contains "$evidence/07-triage-held.txt" 'Auto Review triaging ■ Stop'

    tmux send-keys -t "$session":0 a
    wait_for 'Auto Review stopped' 15
    wait_gone '■ Stop' 15 auto-stopped
    capture_to "$evidence/08-auto-stopped.txt"
    assert_file_contains "$evidence/08-auto-stopped.txt" 'Auto Review stopped' 'Code review'

    tmux send-keys -t "$session":0 h
    wait_for 'Highlight ■ Stop' 15
    capture_to "$evidence/09-highlight-held.txt"
    assert_file_contains "$evidence/09-highlight-held.txt" 'Highlight ■ Stop' 'lib.rs'

    tmux send-keys -t "$session":0 Escape
    wait_for 'Highlight stopped' 15
    wait_gone '■ Stop' 15 esc-precedence
    capture_to "$evidence/10-esc-precedence.txt"
    assert_file_contains "$evidence/10-esc-precedence.txt" 'Highlight stopped' 'Code review'

    wait_log 'kind=triage' 1
    wait_log 'kind=highlight' 1
    release_gate triage
    release_gate highlight
    assert_never_appears late-no-revival 5 'Auto Review triaging' 'Highlight ■ Stop' '· Critical ·'

    tmux send-keys -t "$session":0 a
    wait_for 'Auto Review triaging ■ Stop' 20
    wait_log 'kind=triage' 2
    release_gate triage
    wait_for 'Auto Review 0/1 ■ Stop' 20
    capture_to "$evidence/11-deep-held.txt"
    assert_file_contains "$evidence/11-deep-held.txt" 'Auto Review 0/1 ■ Stop'
    wait_log 'kind=auto' 1
    release_gate auto
    wait_all 25 'Auto review' 'Answer 1 · ops-switch-model' "$CRITICAL_SUMMARY"
    capture_to "$evidence/12-auto-card.txt"
    assert_file_contains "$evidence/12-auto-card.txt" \
        'Auto review' 'Answer 1 · ops-switch-model' "$CRITICAL_SUMMARY" "$CRITICAL_EVIDENCE"
    assert_file_lacks "$evidence/12-auto-card.txt" 'REVIEW-VERDICT' 'severity=' 'Key changes' 'warning' 'info'

    tmux send-keys -t "$session":0 '['
    wait_for '@@ -28,186 +28,1456 @@' 10
    tmux send-keys -t "$session":0 ']'
    wait_for "$DEEP_HEADER" 10
    tmux send-keys -t "$session":0 h
    wait_for 'Highlight ■ Stop' 15
    wait_log 'kind=highlight' 2
    release_gate highlight
    # Auto publishes only the deep hunk, whose header is already
    # "Critical · 2 findings". Highlight completion sets Done and publishes
    # the other hunks without changing the footer. '[' returns to the early
    # hunk; its "Naming · 1 finding" chip exists only after that publish.
    tmux send-keys -t "$session":0 '['
    tmux send-keys -t "$session":0 '['
    wait_for 'Naming · 1 finding' 20
    # Highlight publishes findings without selecting one. n/p walk snapshot
    # order: files, hunks, then row order inside a hunk. That is early naming,
    # middle overdesign, then the deep hunk's critical row before its adjacent
    # naming row. Extra n/p stay on the endpoint.
    tmux send-keys -t "$session":0 n
    wait_for "Finding 1 of 4 · naming · $NAMING_SUMMARY" 20
    capture_to "$evidence/13-finding-early-naming.txt"
    assert_file_contains "$evidence/13-finding-early-naming.txt" \
        "Finding 1 of 4 · naming · $NAMING_SUMMARY" "$EARLY_MARKER" '!'
    assert_file_lacks "$evidence/13-finding-early-naming.txt" 'Key changes' 'warning' 'info'

    tmux send-keys -t "$session":0 n
    wait_for "Finding 2 of 4 · overdesign · $OVERDESIGN_SUMMARY" 10
    capture_to "$evidence/13b-finding-middle-overdesign.txt"
    assert_file_contains "$evidence/13b-finding-middle-overdesign.txt" \
        "Finding 2 of 4 · overdesign · $OVERDESIGN_SUMMARY" "$MIDDLE_TAG"

    tmux send-keys -t "$session":0 n
    wait_for "Finding 3 of 4 · critical · $CRITICAL_SUMMARY" 10
    capture_to "$evidence/13c-finding-deep-critical.txt"
    assert_file_contains "$evidence/13c-finding-deep-critical.txt" \
        "Finding 3 of 4 · critical · $CRITICAL_SUMMARY" "$DEEP_MARKER" '!'

    tmux send-keys -t "$session":0 n
    wait_for "Finding 4 of 4 · naming · $SECOND_SUMMARY" 10
    capture_to "$evidence/14-finding-deep-naming.txt"
    assert_file_contains "$evidence/14-finding-deep-naming.txt" \
        "Finding 4 of 4 · naming · $SECOND_SUMMARY" "$DEEP_SECOND" "$DEEP_MARKER"
    tmux send-keys -t "$session":0 n
    capture_to "$evidence/14b-next-endpoint.txt"
    assert_file_contains "$evidence/14b-next-endpoint.txt" "Finding 4 of 4 · naming · $SECOND_SUMMARY"
    assert_file_lacks "$evidence/14b-next-endpoint.txt" "Finding 1 of 4" "Finding 3 of 4"

    tmux send-keys -t "$session":0 p
    wait_for "Finding 3 of 4 · critical · $CRITICAL_SUMMARY" 10
    tmux send-keys -t "$session":0 p
    tmux send-keys -t "$session":0 p
    wait_for "Finding 1 of 4 · naming · $NAMING_SUMMARY" 10
    tmux send-keys -t "$session":0 p
    capture_to "$evidence/14c-previous-endpoint.txt"
    assert_file_contains "$evidence/14c-previous-endpoint.txt" "Finding 1 of 4 · naming · $NAMING_SUMMARY"
    assert_file_lacks "$evidence/14c-previous-endpoint.txt" "Finding 4 of 4" "Finding 2 of 4"
    tmux send-keys -t "$session":0 h
    wait_for 'Highlight ■ Stop' 15
    tmux send-keys -t "$session":0 h
    wait_for 'Highlight stopped' 15
    wait_gone '■ Stop' 15 highlight-stopped
    capture_to "$evidence/15-highlight-stopped.txt"
    assert_file_contains "$evidence/15-highlight-stopped.txt" 'Highlight stopped' 'Code review'

    tmux send-keys -t "$session":0 h
    wait_for 'Highlight ■ Stop' 15
    wait_log 'kind=highlight' 4
    release_gate highlight
    wait_for 'Highlight failed' 20
    capture_to "$evidence/15b-highlight-rejected.txt"
    assert_file_contains "$evidence/15b-highlight-rejected.txt" \
        'Highlight failed' 'range is not a contiguous numbered side of this hunk'
    assert_file_lacks "$evidence/15b-highlight-rejected.txt" 'STALE-RANGE'
    # p rewrites the footer with the retained finding 1, so the range error
    # is evidence only in the capture above.
    tmux send-keys -t "$session":0 p
    wait_for "Finding 1 of 4 · naming · $NAMING_SUMMARY" 10
    capture_to "$evidence/15b-retained-finding.txt"
    assert_file_contains "$evidence/15b-retained-finding.txt" \
        "Finding 1 of 4 · naming · $NAMING_SUMMARY" "$EARLY_MARKER" '!'
    tmux send-keys -t "$session":0 n
    wait_for "Finding 2 of 4 · overdesign · $OVERDESIGN_SUMMARY" 10
    tmux send-keys -t "$session":0 n
    wait_for "Finding 3 of 4 · critical · $CRITICAL_SUMMARY" 10
    tmux send-keys -t "$session":0 n
    wait_for "Finding 4 of 4 · naming · $SECOND_SUMMARY" 10
    capture_to "$evidence/15c-last-good-findings.txt"
    assert_file_contains "$evidence/15c-last-good-findings.txt" \
        "Finding 4 of 4 · naming · $SECOND_SUMMARY" "$DEEP_SECOND" '!'
    tmux send-keys -t "$session":0 n
    capture_to "$evidence/15d-next-endpoint.txt"
    assert_file_contains "$evidence/15d-next-endpoint.txt" "Finding 4 of 4 · naming · $SECOND_SUMMARY"
    tmux send-keys -t "$session":0 p
    tmux send-keys -t "$session":0 p
    tmux send-keys -t "$session":0 p
    wait_for "Finding 1 of 4 · naming · $NAMING_SUMMARY" 10
    tmux send-keys -t "$session":0 p
    capture_to "$evidence/15e-previous-endpoint.txt"
    assert_file_contains "$evidence/15e-previous-endpoint.txt" "Finding 1 of 4 · naming · $NAMING_SUMMARY"

    # The endpoint check leaves the early hunk selected. Return to the deep
    # thread before the comment composer, or c opens Comment 1 there.
    tmux send-keys -t "$session":0 ']'
    tmux send-keys -t "$session":0 ']'
    wait_for "$DEEP_HEADER" 10
    tmux send-keys -t "$session":0 c
    wait_for ' Comment · Enter submit · Esc cancel ' 10
    tmux send-keys -t "$session":0 -l "$COMMENT_SENTINEL"
    wait_for "$COMMENT_SENTINEL" 10
    capture_to "$evidence/16-composer.txt"
    assert_file_contains "$evidence/16-composer.txt" ' Comment · Enter submit · Esc cancel ' "$COMMENT_SENTINEL"

    tmux send-keys -t "$session":0 Escape
    wait_gone 'Enter submit' 10 composer-cancel
    capture_to "$evidence/17-composer-cancel.txt"
    assert_file_lacks "$evidence/17-composer-cancel.txt" 'Enter submit' "$COMMENT_SENTINEL"
    assert_file_contains "$evidence/17-composer-cancel.txt" 'Code review'

    tmux send-keys -t "$session":0 c
    wait_for ' Comment · Enter submit · Esc cancel ' 10
    tmux send-keys -t "$session":0 -l "$COMMENT_SENTINEL"
    tmux send-keys -t "$session":0 Enter
    wait_all 25 'Comment 2' "$MANUAL_REPLY" 'Answer 2 · ops-switch-model'
    capture_to "$evidence/18-manual-comment.txt"
    assert_file_contains "$evidence/18-manual-comment.txt" 'Comment 2' "$MANUAL_REPLY" 'Answer 2 · ops-switch-model'

    tmux send-keys -t "$session":0 Space
    wait_for '· folded' 10
    capture_to "$evidence/19-folded.txt"
    assert_file_contains "$evidence/19-folded.txt" '· 4 comments · folded'

    tmux send-keys -t "$session":0 q
    wait_gone 'review-base → working tree' 15 closed
    capture_to "$evidence/20-closed.txt"
    assert_file_lacks "$evidence/20-closed.txt" 'review-base → working tree' 'focus:tree' 'focus:diff'

    tmux send-keys -t "$session":0 -l '/quit'
    wait_for '/quit' 10
    tmux send-keys -t "$session":0 Enter
    if ! tmux_wait_for "$session" 20 '===TUI-DONE-code-review-operations===' >"$evidence/21-quit.txt"; then
        fail "user.$SCENARIO: TUI did not exit after /quit (see $evidence/21-quit.txt)"
    fi

    local summary_parent="$root/summary" summary_workspace="$root/summary/workspace" staged_needle unstaged_needle before_scroll after_scroll
    seed_summary_fixture "$summary_parent" "$summary_workspace"
    tmux respawn-pane -k -c "$summary_workspace" -t "$session":0 \
        "$(rpi_pane_command '===TUI-DONE-code-review-summary===')"
    wait_for "$PROVIDER/$MODEL_ID" 30
    tmux send-keys -t "$session":0 Escape
    sleep 0.3
    tmux send-keys -t "$session":0 -l '/review HEAD'
    wait_for '/review HEAD' 10
    tmux send-keys -t "$session":0 Enter
    wait_for 'Code review · HEAD → working tree' 30
    curl -s --max-time 5 -X POST "http://127.0.0.1:${mock_port}/__tldr-late" -d '{}' >"$evidence/tldr-late.txt"
    tmux send-keys -t "$session":0 s
    wait_for 'Summary' 20
    wait_for '■ Stop' 20
    wait_log 'kind=tldr' 1 45
    wait_log 'tldr generation=1' 1 45
    wait_log 'tldr-late=consumed' 1
    wait_log 'tldr gate=entered' 1
    tmux send-keys -t "$session":0 s
    wait_for 'TLDR stopped' 15
    advance_summary_generation
    stopped=0
    while [ "$(log_count 'tldr gate=entered')" -gt "$stopped" ]; do
        stopped=$((stopped + 1))
        release_gate_to tldr "$evidence/release-tldr-$stopped.txt"
        grep -q '"delivered":false' "$evidence/release-tldr-$stopped.txt" || fail "user.$SCENARIO: stopped summary batch was delivered (see $evidence/release-tldr-$stopped.txt)"
        grep -q '"outcome":"peer_closed"' "$evidence/release-tldr-$stopped.txt" || fail "user.$SCENARIO: stopped summary batch was not peer_closed (see $evidence/release-tldr-$stopped.txt)"
    done
    [ "$stopped" -ge 1 ] || fail "user.$SCENARIO: first summary stop closed no live request"
    assert_never_appears summary-stop 4 'STALE-LATE-SNAPSHOT'
    tmux send-keys -t "$session":0 s
    wait_for 'Summary' 20
    wait_for '■ Stop' 20
    for expected_batch in 1 2 3 4 5; do
        wait_log "tldr generation=2 batch=$expected_batch" 1 45
        release_gate_to tldr "$evidence/release-tldr-good-$expected_batch.txt"
        grep -q '"delivered":true' "$evidence/release-tldr-good-$expected_batch.txt" || fail "user.$SCENARIO: summary batch $expected_batch was not delivered (see $evidence/release-tldr-good-$expected_batch.txt)"
        grep -q '"outcome":"delivered"' "$evidence/release-tldr-good-$expected_batch.txt" || fail "user.$SCENARIO: summary batch $expected_batch outcome was not delivered (see $evidence/release-tldr-good-$expected_batch.txt)"
    done
    wait_for 'Purpose: root0 compared ahead 1' 20
    wait_gone 'Summary running' 20 summary-settled
    capture_to "$evidence/19a-summary-settled.txt"
    assert_file_lacks "$evidence/19a-summary-settled.txt" 'Summary running' '■ Stop'
    # Normal reading flow: PageUp returns the overlay to its top; the first
    # captured frame must actually show the first purpose, proving the top
    # rather than assuming the key count. Each PageDown then keeps its frame
    # until the capture changes (same 5s deadline as wait_for) or every purpose
    # has been seen. A stuck overlay still fails; a repeated frame at the
    # bottom never does because the clean purpose ended the scan already.
    : >"$evidence/19b-summary.txt"
    for _ in $(seq 1 8); do
        tmux send-keys -t "$session":0 PageUp
    done
    sleep 0.3
    summary_frame="$(pane_capture)"
    case "$summary_frame" in
        *"Purpose: root0 compared ahead 1"*) ;;
        *) fail "user.$SCENARIO: PageUp did not reach the summary top (see $evidence/19b-summary.txt)" ;;
    esac
    for summary_page in $(seq 1 20); do
        printf '%s\n' "$summary_frame" >>"$evidence/19b-summary.txt"
        printf '\n--- summary frame %s ---\n' "$summary_page" >>"$evidence/19b-summary.txt"
        case "$summary_frame" in
            *"Purpose: root0/clean compared ahead 0"*) break ;;
        esac
        [ "$summary_page" -lt 20 ] || fail "user.$SCENARIO: summary did not reach root0/clean within 20 frames (see $evidence/19b-summary.txt)"
        summary_previous="$summary_frame"
        tmux send-keys -t "$session":0 PageDown
        summary_deadline=$((SECONDS + 5))
        while [ "$summary_frame" = "$summary_previous" ] && [ "$SECONDS" -lt "$summary_deadline" ]; do
            sleep 0.2
            summary_frame="$(pane_capture)"
        done
        [ "$summary_frame" != "$summary_previous" ] || fail "user.$SCENARIO: summary overlay did not change within 5s before root0/clean (see $evidence/19b-summary.txt)"
    done
    case "$summary_frame" in
        *"Purpose: root0/clean compared ahead 0"*) ;;
        *) fail "user.$SCENARIO: summary did not reach root0/clean within 20 frames (see $evidence/19b-summary.txt)" ;;
    esac
    printf '%s\n' "$summary_frame" >"$evidence/19c-summary-scrolled.txt"
    assert_file_contains "$evidence/19b-summary.txt" \
        'Purpose: root0 compared ahead 1' \
        'Purpose: root0/bare no_origin ahead 0' \
        'Purpose: root0/big no_origin ahead 0' \
        'origin: no origin' \
        'Purpose: root0/clean compared ahead 0' \
        'Purpose: modified left.txt +18500/-1 LEFTTOK' \
        'Purpose: modified right.txt +18500/-1 RIGHTTOK' \
        'ahead 1' 'pixel.png' 'binary' 'loose.txt' \
        'A added.txt' 'D kept.txt' 'R old-name.txt -> new-name.txt +0/-0' \
        '5 staged, 1 unstaged, 1 untracked, 1 commits'
    staged_needle="$(sed -n '1p' "$summary_parent/summary-numstat.txt")"
    unstaged_needle="$(sed -n '2p' "$summary_parent/summary-numstat.txt")"
    assert_file_contains "$evidence/19b-summary.txt" "$staged_needle" "$unstaged_needle"
    if [ "$staged_needle" = "$unstaged_needle" ]; then
        [ "$(grep -F -o -e "$staged_needle" "$evidence/19b-summary.txt" | wc -l)" -ge 2 ] || fail "user.$SCENARIO: both.rs staged and unstaged counts collapsed"
    fi
    assert_file_lacks "$evidence/19b-summary.txt" 'Write one Markdown document' 'STALE-LATE-SNAPSHOT' 'exceeded 2097152' '##' 'LEFT-HEAD-MARKER' 'RIGHT-TAIL-MARKER'
    tmux send-keys -t "$session":0 x
    wait_gone 'Purpose: root0 compared ahead 1' 10 summary-hidden
    capture_to "$evidence/19d-summary-hidden.txt"
    assert_file_lacks "$evidence/19d-summary-hidden.txt" 'Purpose: root0 compared ahead 1'
    tmux send-keys -t "$session":0 q
    wait_gone 'HEAD → working tree' 15 summary-closed
    capture_to "$evidence/20b-summary-closed.txt"
    assert_file_lacks "$evidence/20b-summary-closed.txt" 'HEAD → working tree' 'focus:tree' 'focus:diff'
    tmux send-keys -t "$session":0 -l '/quit'
    wait_for '/quit' 10
    tmux send-keys -t "$session":0 Enter
    if ! tmux_wait_for "$session" 20 '===TUI-DONE-code-review-summary===' >"$evidence/22-summary-quit.txt"; then
        fail "user.$SCENARIO: summary TUI did not exit after /quit (see $evidence/22-summary-quit.txt)"
    fi


    assert_large_summary_proof "$summary_parent/summary-big-expected.json"
    assert_mock_counts
    printf 'user.%s passed\nevidence=%s\n' "$SCENARIO" "$evidence"
}

main "$@"