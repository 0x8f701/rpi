#!/usr/bin/env bash
# Web code-review OPERATIONS lane (playwright-only hard gate). rpi --listen with --approve; strict-schema provider is served by the mjs on its locked ephemeral mock-port.txt port (baked into models.json).
set -euo pipefail
SCRIPT_DIR="$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
E2E_DIR="$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd -P)"
# shellcheck source=../lib/common.sh
. "$E2E_DIR/lib/common.sh"
# shellcheck source=lib/fixture.sh
. "$SCRIPT_DIR/lib/fixture.sh"

SCENARIO="web-code-review-operations"
TOKEN="web-code-review-operations-e2e-token-$$-$(date +%s)"
DIRTY_FILE="app.rs"
CRIT_FILE="crit.rs"
WARN_FILE="warn.rs"
INFO_FILE="info.rs"
PREVIEW_FILE="notes.md"
ATTACH_FILE="shot.png"

start_provider() {
    local root="$1" evidence="$2" deadline
    RPI_SUMMARY_TRUTH="$root/summary-git-truth.json" \
    node "$SCRIPT_DIR/code_review_operations_test.mjs" --provider "$root/mock-port.txt" \
        >"$evidence/provider.log" 2>&1 &
    register_pid "$!"
    deadline=$((SECONDS + 20))
    while [ ! -s "$root/mock-port.txt" ] && [ "$SECONDS" -lt "$deadline" ]; do sleep 0.2; done
    [ -s "$root/mock-port.txt" ] || fail "$SCENARIO: provider did not write its port file"
}

write_models() {
    local root="$1" port="$2"
    mkdir -p "$root/home/.rpi"
    cat >"$root/home/.rpi/models.json" <<EOF
{
  "providers": {
    "user-steering": {
      "baseUrl": "http://127.0.0.1:$port",
      "api": "openai-completions",
      "models": [
        { "id": "mock", "name": "Steering Mock", "contextWindow": 1000000, "maxTokens": 8192 },
        { "id": "mock-2", "name": "Steering Mock Two", "contextWindow": 1000000, "maxTokens": 8192 }
      ]
    }
  }
}
EOF
}

spawn_rpi() {
    local root="$1" evidence="$2"
    local stdin_path="$root/stdin"
    rm -f "$stdin_path"
    mkfifo "$stdin_path"
    exec 9<>"$stdin_path"
    (
        cd "$root/workspace"
        exec env -i \
            HOME="$root/home" USERPROFILE="$root/home" \
            PATH="${PATH:-/usr/bin:/bin}" LANG="${LANG:-C.UTF-8}" LC_ALL="${LC_ALL:-C.UTF-8}" \
            RPI_HOME="$root/home/.rpi" \
            RPI_OFFLINE=1 RPI_SKIP_VERSION_CHECK=1 \
            RPI_WEB_DEV_DIR="${RPI_WEB_DEV_DIR:-}" \
            "$RPI_BIN" --offline --approve \
            --listen 127.0.0.1:0 \
            --listen-plaintext \
            --listen-token-file "$root/token" \
            --model user-steering/mock --api-key user-mock-key \
            <"$stdin_path" >"$evidence/rpi.stdout" 2>"$evidence/rpi.stderr"
    ) &
    RPI_PID=$!
    register_pid $!
}

seed_operations_fixture() {
    local workspace="$1"
    local parent
    parent="$(CDPATH= cd -- "$workspace/.." && pwd -P)"
    local gitenv=(env GIT_CONFIG_NOSYSTEM=1 GIT_CONFIG_GLOBAL="$parent/absent-global-gitconfig" GIT_CONFIG_COUNT=0)
    "${gitenv[@]}" git -C "$workspace" init -q
    "${gitenv[@]}" git -C "$workspace" config user.email "web-e2e@example.com"
    "${gitenv[@]}" git -C "$workspace" config user.name "Web E2E"
    "${gitenv[@]}" git -C "$workspace" config commit.gpgsign false
    "${gitenv[@]}" git -C "$workspace" config protocol.file.allow always
    local origin="$parent/origin.git" ahead head_oid origin_oid
    "${gitenv[@]}" git init -q --bare "$origin"
    "${gitenv[@]}" git -C "$workspace" remote add origin "$origin"
    cat >"$workspace/$DIRTY_FILE" <<'EOF'
alpha
beta
gamma
delta
EOF
    cat >"$workspace/$CRIT_FILE" <<'EOF'
fn crit_one() {}
fn crit_two() {}
EOF
    cat >"$workspace/$WARN_FILE" <<'EOF'
warn base line one
warn base line two
EOF
    cat >"$workspace/$INFO_FILE" <<'EOF'
info base alpha
info base beta
EOF
    printf '# Fixture notes\n\n**bold** preview target.\n\n- item one\n- item two\n' >"$workspace/$PREVIEW_FILE"
    python3 -c 'import pathlib,base64,sys; pathlib.Path(sys.argv[1]).write_bytes(base64.b64decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg=="))' "$workspace/$ATTACH_FILE"
    "${gitenv[@]}" git -C "$workspace" add -- .
    "${gitenv[@]}" git -C "$workspace" -c commit.gpgsign=false commit -q -m baseline
    cat >"$workspace/$DIRTY_FILE" <<'EOF'
alpha
BETA
gamma
delta
EOF
    cat >"$workspace/$CRIT_FILE" <<'EOF'
fn crit_one() { todo!(); }
fn data() {}
EOF
    cat >"$workspace/$WARN_FILE" <<'EOF'
warn base line one
WARN-CHANGED
warn base line two
EOF
    cat >"$workspace/$INFO_FILE" <<'EOF'
info base alpha
INFO-BETA
EOF
    printf '# Fixture notes\n\n**bold** preview target.\n\n- item one\n- item two\n- PREVIEW-EXTRA line\n' >"$workspace/$PREVIEW_FILE"
    # Summary ground truth, beside the staged selected diff above.
    # primary is ahead of the local bare origin by one commit. both.rs is MM.
    # kept.txt is deleted. old-name.txt -> new-name.txt is a rename.
    # pixel.png is committed on origin, modified in the ahead commit, then staged again.
    # bare/ has no origin. clean/ matches its own local bare origin.
    mkdir -p "$workspace/bare" "$workspace/clean"
    printf 'both staged\nboth base\n' >"$workspace/both.rs"
    printf 'kept base\n' >"$workspace/kept.txt"
    printf 'rename base\n' >"$workspace/old-name.txt"
    printf 'ahead base\n' >"$workspace/ahead.txt"
    printf 'shared base\n' >"$workspace/bare/shared.txt"
    printf 'clean base\n' >"$workspace/clean/clean.txt"
    python3 -c 'import pathlib,sys; pathlib.Path(sys.argv[1]).write_bytes(bytes([0,1,2,3]))' "$workspace/pixel.png"
    "${gitenv[@]}" git -C "$workspace" add -- both.rs kept.txt old-name.txt ahead.txt pixel.png
    "${gitenv[@]}" git -C "$workspace" -c commit.gpgsign=false commit -q -m "summary base"
    "${gitenv[@]}" git -C "$workspace" branch -M review-base
    "${gitenv[@]}" git -C "$workspace" push -q origin review-base
    printf 'ahead only\n' >>"$workspace/ahead.txt"
    python3 -c 'import pathlib,sys; pathlib.Path(sys.argv[1]).write_bytes(bytes([0,1,2,255]))' "$workspace/pixel.png"
    "${gitenv[@]}" git -C "$workspace" add -- ahead.txt pixel.png
    "${gitenv[@]}" git -C "$workspace" -c commit.gpgsign=false commit -q -m "summary ahead"
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
    "${gitenv[@]}" git -C "$workspace/bare" config user.email "web-e2e@example.com"
    "${gitenv[@]}" git -C "$workspace/bare" config user.name "Web E2E"
    "${gitenv[@]}" git -C "$workspace/bare" config commit.gpgsign false
    "${gitenv[@]}" git -C "$workspace/bare" add -- shared.txt
    "${gitenv[@]}" git -C "$workspace/bare" -c commit.gpgsign=false commit -q -m "bare base"
    "${gitenv[@]}" git -C "$workspace/clean" init -q
    "${gitenv[@]}" git -C "$workspace/clean" config user.email "web-e2e@example.com"
    "${gitenv[@]}" git -C "$workspace/clean" config user.name "Web E2E"
    "${gitenv[@]}" git -C "$workspace/clean" config commit.gpgsign false
    "${gitenv[@]}" git -C "$workspace/clean" config protocol.file.allow always
    "${gitenv[@]}" git -C "$workspace/clean" add -- clean.txt
    "${gitenv[@]}" git -C "$workspace/clean" -c commit.gpgsign=false commit -q -m "clean base"
    "${gitenv[@]}" git -C "$workspace/clean" branch -M review-base
    "${gitenv[@]}" git init -q --bare "$workspace/../clean-origin.git"
    "${gitenv[@]}" git -C "$workspace/clean" remote add origin "$workspace/../clean-origin.git"
    "${gitenv[@]}" git -C "$workspace/clean" push -q origin review-base
    ahead="$("${gitenv[@]}" git -C "$workspace" rev-list --count origin/review-base..HEAD)"
    [ "$ahead" = 1 ] || fail "$SCENARIO: primary is not exactly one commit ahead (${ahead:-empty})"
    head_oid="$("${gitenv[@]}" git -C "$workspace" rev-parse HEAD)"
    origin_oid="$("${gitenv[@]}" git -C "$workspace" rev-parse origin/review-base)"
    [ "$head_oid" != "$origin_oid" ] || fail "$SCENARIO: primary HEAD matches origin"
    case "$("${gitenv[@]}" git -C "$workspace" status --porcelain=v1)" in
        *"MM both.rs"*) ;;
        *) fail "$SCENARIO: both.rs is not MM" ;;
    esac
    case "$("${gitenv[@]}" git -C "$workspace" status --porcelain=v1)" in
        *"A  added.txt"*) ;;
        *) fail "$SCENARIO: added.txt is not staged added" ;;
    esac
    case "$("${gitenv[@]}" git -C "$workspace" status --porcelain=v1)" in
        *"D  kept.txt"*) ;;
        *) fail "$SCENARIO: kept.txt is not staged deleted" ;;
    esac
    case "$("${gitenv[@]}" git -C "$workspace" status --porcelain=v1)" in
        *"R  old-name.txt -> new-name.txt"*) ;;
        *) fail "$SCENARIO: rename endpoints missing" ;;
    esac
    case "$("${gitenv[@]}" git -C "$workspace" status --porcelain=v1)" in
        *"M  pixel.png"*) ;;
        *) fail "$SCENARIO: pixel.png is not a staged modification" ;;
    esac
    case "$("${gitenv[@]}" git -C "$workspace" status --porcelain=v1)" in
        *"?? loose.txt"*) ;;
        *) fail "$SCENARIO: loose.txt is not untracked" ;;
    esac
    [ -z "$("${gitenv[@]}" git -C "$workspace/bare" status --porcelain=v1)" ] || fail "$SCENARIO: bare is not clean"
    [ -z "$("${gitenv[@]}" git -C "$workspace/bare" remote)" ] || fail "$SCENARIO: bare has an origin"
    [ -z "$("${gitenv[@]}" git -C "$workspace/clean" status --porcelain=v1)" ] || fail "$SCENARIO: clean is not clean"
    [ "$("${gitenv[@]}" git -C "$workspace/clean" rev-list --count origin/review-base..HEAD)" = 0 ] || fail "$SCENARIO: clean is ahead"
    pixel_commit="$("${gitenv[@]}" git -C "$workspace" show --numstat --format= -1 -- pixel.png)"
    pixel_staged="$("${gitenv[@]}" git -C "$workspace" diff --cached --numstat -- pixel.png)"
    both_staged="$("${gitenv[@]}" git -C "$workspace" diff --cached --numstat -- both.rs)"
    both_unstaged="$("${gitenv[@]}" git -C "$workspace" diff --numstat -- both.rs)"
    added_staged="$("${gitenv[@]}" git -C "$workspace" diff --cached --numstat -- added.txt)"
    deleted_staged="$("${gitenv[@]}" git -C "$workspace" diff --cached --numstat -- kept.txt)"
    python3 - "$parent/summary-git-truth.json" "$head_oid" "$origin_oid" "$ahead" "$both_staged" "$both_unstaged" "$added_staged" "$deleted_staged" "$pixel_commit" "$pixel_staged" <<'PY'
import json, pathlib, sys
path, head, origin, ahead, both_staged, both_unstaged, added_staged, deleted_staged, pixel_commit, pixel_staged = sys.argv[1:]
def pair(row, label):
    added, deleted, *_rest = row.split()
    if not added.isdigit() or not deleted.isdigit():
        raise SystemExit(f'{label} numstat is not text: {row}')
    return f'+{added}/-{deleted}'
def binary(row, label):
    added, deleted, name = row.split()
    if added != '-' or deleted != '-' or name != 'pixel.png':
        raise SystemExit(f'{label} is not a binary pixel.png numstat: {row}')
    return 'binary'
staged_pair, unstaged_pair = pair(both_staged, 'staged both.rs'), pair(both_unstaged, 'unstaged both.rs')
if staged_pair == unstaged_pair:
    raise SystemExit('both.rs layers have the same numstat')
pathlib.Path(path).write_text(json.dumps({
    "primary": {"head": head, "origin": origin, "ahead": int(ahead), "behind": 0},
    "labels": ["root0", "root0/bare", "root0/big", "root0/clean"],
    "both": {"staged": staged_pair, "unstaged": unstaged_pair},
    "added": pair(added_staged, "added.txt"),
    "deleted": pair(deleted_staged, "kept.txt"),
    "pixel": {"committed": binary(pixel_commit, "committed pixel.png"), "staged": binary(pixel_staged, "staged pixel.png")},
}, indent=2) + "\n", encoding="utf-8")
PY
    "${gitenv[@]}" git -C "$workspace" add -- "$DIRTY_FILE" "$CRIT_FILE" "$WARN_FILE" "$INFO_FILE" "$PREVIEW_FILE"
    # Cumulative >2 MiB summary fixture. Two individually legal real patches in
    # their own nested repo: the review snapshot excludes nested repos, so the
    # selected diff/hunk fixtures above keep their exact counts while summary
    # discovery finds root0/big and must split its two ~1.1 MB records into
    # separate provider batches.
    local big="$workspace/big"
    mkdir -p "$big"
    "${gitenv[@]}" git -C "$big" init -q
    "${gitenv[@]}" git -C "$big" config user.email "web-e2e@example.com"
    "${gitenv[@]}" git -C "$big" config user.name "Web E2E"
    "${gitenv[@]}" git -C "$big" config commit.gpgsign false
    printf 'base\n' >"$big/left.txt"
    printf 'base\n' >"$big/right.txt"
    "${gitenv[@]}" git -C "$big" add -A
    "${gitenv[@]}" git -C "$big" -c commit.gpgsign=false commit -q -m "big base"
    python3 - "$big" <<'PY'
import pathlib, sys
big = pathlib.Path(sys.argv[1])
for tag, name in (("LEFT", "left.txt"), ("RIGHT", "right.txt")):
    lines = []
    for index in range(18500):
        if index == 0:
            text = f"{tag}-HEAD-MARKER"
        elif index == 9250:
            text = f"{tag}-MIDDLE-MARKER"
        elif index == 18499:
            text = f"{tag}-TAIL-MARKER"
        else:
            text = f"{tag}-{index:05d} " + "x" * 48
        lines.append(text + "\n")
    (big / name).write_text("".join(lines), encoding="utf-8")
PY
    [ "$(wc -l <"$big/left.txt")" -eq 18500 ] || fail "$SCENARIO: left.txt fixture length"
    [ "$(wc -l <"$big/right.txt")" -eq 18500 ] || fail "$SCENARIO: right.txt fixture length"
    # Precompute each collector patch byte count, sha256, and exact HEAD/MIDDLE/
    # TAIL byte offsets from real git output before any model request exists.
    # Only safe counts and digests are recorded here, never the patch source.
    python3 - "$big" "$parent/summary-git-truth.json" "$parent/absent-global-gitconfig" <<'PY'
import hashlib, json, os, pathlib, subprocess, sys
big, truth_path, global_config = sys.argv[1:]
environment = dict(os.environ, GIT_CONFIG_NOSYSTEM="1", GIT_CONFIG_GLOBAL=global_config, GIT_CONFIG_COUNT="0", LC_ALL="C")
def run(*args):
    return subprocess.run(["git", "-C", big, *args], env=environment, check=True, capture_output=True).stdout
files, total = {}, 0
for name, tag, token in (("left.txt", "LEFT", "LEFTOK"), ("right.txt", "RIGHT", "RIGHTOK")):
    text = run("--literal-pathspecs", "diff", "--no-ext-diff", "--no-textconv", "--no-color", "--find-renames", "--", name).decode("utf-8")
    lines = text.split("\n")
    if lines and lines[-1] == "":
        lines.pop()
    first = next(index for index, line in enumerate(lines) if line.startswith("@@"))
    patch = "\n".join(lines[first:])
    encoded = patch.encode("utf-8")
    markers = {}
    for marker, key in (("HEAD", "head"), ("MIDDLE", "middle"), ("TAIL", "tail")):
        needle = f"+{tag}-{marker}-MARKER"
        if patch.count(needle) != 1:
            raise SystemExit(f"{name}: {marker} marker is not unique")
        markers[key] = encoded.index(needle.encode("utf-8"))
    if not markers["head"] < markers["middle"] < markers["tail"]:
        raise SystemExit(f"{name}: markers are not ordered")
    if not patch.endswith(f"+{tag}-TAIL-MARKER"):
        raise SystemExit(f"{name}: patch does not end on the tail marker")
    numstat = run("--literal-pathspecs", "diff", "--numstat", "--no-ext-diff", "--no-textconv", "--find-renames", "--", name).decode("utf-8").split()
    if len(numstat) < 3 or not numstat[0].isdigit() or not numstat[1].isdigit():
        raise SystemExit(f"{name}: numstat is not text: {numstat}")
    if len(encoded) >= 2 * 1024 * 1024:
        raise SystemExit(f"{name}: one patch already exceeds the 2 MiB batch bound")
    files[name] = {
        "tag": tag,
        "token": token,
        "numstat": f"+{numstat[0]}/-{numstat[1]}",
        "patchBytes": len(encoded),
        "patchSha256": hashlib.sha256(encoded).hexdigest(),
        "markers": markers,
    }
    total += len(encoded)
if total <= 2 * 1024 * 1024:
    raise SystemExit(f"large fixture total {total} does not exceed 2 MiB")
if files["left.txt"]["patchSha256"] == files["right.txt"]["patchSha256"]:
    raise SystemExit("large fixture patches are identical")
truth = json.loads(pathlib.Path(truth_path).read_text(encoding="utf-8"))
truth["large"] = {"repo": "root0/big", "totalPatchBytes": total, "files": files}
pathlib.Path(truth_path).write_text(json.dumps(truth, indent=2) + "\n", encoding="utf-8")
PY
}

main() {
    case "${1:-run}" in
        list|--list|--dry-run)
            printf '%s\n' \
                'web-code-review-operations - review operations with a deterministic strict-schema model boundary: /code-review <base> opens with default 中文 + model picker; Auto Review Stage 1 schema triage -> selected Stage 2 review_findings calls scope exactly one hunk identity from the prompt and publish reviewFindings; Highlight review_findings covers every supplied hunk including findings=[] and paints exact row marks (critical/overdesign/naming, old-side deletion) with previous/next disabled at the endpoints and no whole-hunk tint; an out-of-range findings reply fails with exactly one error and keeps last-good; TLDR is plaintext with no tools, Expand details unclips the same Markdown body including Mermaid, and Stop rejects a late stale snapshot; Source|Preview flips on markdown; the running `■ Stop` stops ONLY the active operation (autoReview/highlight/tldr) and Esc stops the active operation first; late responses after Stop never republish; user-action RPC failures surface exactly ONE App toast and ZERO panel/sidebar error nodes; review composer geometry/hover/focus + real PNG picker/chip/remove (no submit) and enabled mic a11y; desktop >=1280 + mobile 390x844 screenshots; zero page/console errors; zero horizontal overflow (PLAYWRIGHT-ONLY, hard gate, UNEXECUTED until main runs it)'
            return 0
            ;;
        run|all) ;;
        *) fail "usage: $0 [run|list]" ;;
    esac

    require_rpi
    web_require_browser
    require_cmd git
    require_cmd python3

    local root evidence url
    root="$(scenario_workspace "$SCENARIO")"
    evidence="$EVIDENCE_ROOT/$SCENARIO"
    printf '%s\n' "$TOKEN" >"$root/token"
    start_provider "$root" "$evidence"
    write_models "$root" "$(cat "$root/mock-port.txt")"
    seed_operations_fixture "$root/workspace"
    spawn_rpi "$root" "$evidence"
    url="$(web_wait_for_listener "$evidence")"
    log "$SCENARIO: listener at $url"

    local pw_status=0
    web_run_playwright "$url" "$evidence" "$root/playwright" \
        "$SCRIPT_DIR/code_review_operations_test.mjs" \
        RPI_WORKSPACE="$root/workspace" \
        RPI_DIRTY_FILE="$DIRTY_FILE" \
        RPI_CRIT_FILE="$CRIT_FILE" \
        RPI_WARN_FILE="$WARN_FILE" \
        RPI_INFO_FILE="$INFO_FILE" \
        RPI_PREVIEW_FILE="$PREVIEW_FILE" \
        RPI_ATTACH_FILE="$ATTACH_FILE" || pw_status=$?
    if [ "$pw_status" -eq 0 ]; then
        printf 'playwright: code-review operations (strict-schema triage + single-hunk auto findings; highlight exact row marks critical/overdesign/naming including old-side deletion and clean hunks; previous/next endpoints; invalid range preserves last-good; TLDR expand/Mermaid/Stop/late snapshot; no Key changes card; Source|Preview; targeted Stop + Esc-first; late-response non-revival; exactly-one-toast failures; composer PNG picker/chip/remove + enabled mic; desktop+mobile shots; zero overflow/browser errors): PASSED\n' \
            | tee "$evidence/playwright-summary.txt"
    elif [ "$pw_status" -eq 1 ]; then
        fail "$SCENARIO: playwright SETUP FAILED (node/chromium/npm unavailable)"
    else
        fail "$SCENARIO: playwright lane failed (exit $pw_status)"
    fi

    web_sanity_http "$url"
    web_finish_lane "$SCENARIO"
}

main "$@"
