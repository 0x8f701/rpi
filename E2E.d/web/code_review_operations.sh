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
        { "id": "mock", "name": "Steering Mock", "contextWindow": 32768, "maxTokens": 2048 },
        { "id": "mock-2", "name": "Steering Mock Two", "contextWindow": 32768, "maxTokens": 2048 }
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
    git -C "$workspace" init -q
    git -C "$workspace" config user.email "web-e2e@example.com"
    git -C "$workspace" config user.name "Web E2E"
    git -C "$workspace" config commit.gpgsign false
    git -C "$workspace" config protocol.file.allow always
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
    git -C "$workspace" add -- .
    git -C "$workspace" -c commit.gpgsign=false commit -q -m baseline
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
    git -C "$workspace" add -- "$DIRTY_FILE" "$CRIT_FILE" "$WARN_FILE" "$INFO_FILE" "$PREVIEW_FILE"
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
