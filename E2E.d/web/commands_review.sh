#!/usr/bin/env bash
# Web composer command-button + code-review panel E2E lane
# (playwright-only hard gate).
#
# Spawns the real `rpi --listen` binary with a token file and the loopback
# mock provider (--scenario steering), opens `/web` in a real browser, and
# asserts the v0.2.11 Web composer command surface + the real code-review
# panel against a seeded TEMP git repo + a fixture project skill:
#   - #command-btn sits LEFT of #prompt-input in the composer row
#   - opening the command picker shows /compact, /skill, /code-review
#     (the backend get_commands catalog is authoritative)
#   - choosing /code-review inserts the draft into the composer WITHOUT
#     auto-submitting (no user bubble, panel still closed)
#   - Enter dispatches code_review_open and the real review panel renders the
#     HEAD→working-tree comparison label, the dirty file, the changed
#     diff lines, and the dirty Solidity fixture's lexed .sol diff
#     (contract/function/type tokens over exact preserved text, desktop +
#     mobile); explicit hunk selection
#     (never auto-selected), the file filter, keyboard file navigation, the
#     oversize fixture's eager per-file page chain, Enter comment submit
#     (Shift+Enter newline; a composing Enter never submits) +
#     streaming/queued-comment/abort, the inline close-confirm guard
#     (including immediate Close/Refresh while the comment textarea still
#     holds focus — no blur/hunk switch; Close panel emits exactly one
#     owner-stamped code_review_close, zero before the primary click, no
#     duplicate close and no snapshot frames over a ≥3.6s post-close window),
#     Escape close, mobile Files/Diff/Thread tab transitions, and closing
#     the panel removes it from the DOM
#   - the dirty file carries TWO separated diff hunks; a comment on the
#     SECOND hunk lands only in that hunk's thread (identity-keyed), the
#     per-hunk drafts A/B survive hunk switches, and submitting A leaves B
#   - TUI/Web parity for the changed-file tree: compact colored status
#     glyphs (M/A), basename-only rows with the full repo-relative path in
#     data-file-path/title and the readable full state in aria-label,
#     selection/filter keyed on the full path, and no rail overflow on
#     desktop or mobile (stats never swallow the filename)
#   - switching files clears the composer/hunk selection
#   - hostile diff/comment text (`<script>` + `<img onerror>` markers) stays
#     literal text — no element is created and no script runs (no dialog)
#   - the panel polls code_review_snapshot at the 1.5s contract while open
#     (≥2 frames ~1.5s apart observed) and polling stops after panel close
#     (≥3.6s with zero snapshot frames and no duplicate code_review_close)
#   - a session switch closes the owning review workspace via a stamped
#     code_review_close (sessionId A), and a bare /code-review in the target
#     session never reuses the previous session's revision args
#   - a warm reconnect (forced socket close + auto-reconnect) whose displayed
#     session is a sibling of the listener primary keeps the slash on the
#     DISPLAYED session: /code-review dispatches code_review_open stamped with
#     that session (never the re-bound primary), the review is served by it,
#     and no invalid-revision toast appears
#   - review ownership across a cross-session switch (A → B → A): with the
#     display authority settled on the owning session, the restored panel
#     dispatches a bare owner-owned code_review_open that succeeds with its
#     working-tree snapshot, carries no revisions from the session the user
#     left, and issues no live code_review_* frame naming another session
#   - review ownership with the active ref diverged from the display: a
#     reconnect that still displays the review's owner keeps the panel mounted,
#     and leaving that session for a third session closes its review on it
#     (probe-verified) while the third session's restored panel opens its own
#     bare scope with no foreign revisions
#   - owner-targeted cleanup inside a pending switch: the sibling's catch-up and
#     switch cuts are held so the negotiated handoff keeps its pending window,
#     and the deferred probe resolving inside it closes the review exactly once
#     on the session being left (probe-verified open-free)
#   - the mounted panel's owner close across that pending window: a negotiated
#     handoff unmounts the review panel and its owner-stamped code_review_close
#     still lands exactly once (probe-verified open-free, no close-failure
#     toast)
#   - a restored panel recovers its own bootstrap after that settle: a
#     successful bare target-owned code_review_open with its working-tree
#     snapshot applied, the model catalog recovered, and no switch-pending
#     toast
#   - the literal single-ref user path `/code-review origin/master` (the
#     fixture seeds refs/remotes/origin/master LOCALLY via git update-ref —
#     no remote URL, no network) dispatches exactly one code_review_open
#     with from=origin/master includeWorkingTree=true and no `to` key, the
#     panel label names origin/master→working tree over a non-empty diff,
#     the fullscreen shell holds on desktop AND mobile, and close restores
#     the prior app layout
#   - /skill <fixture> renders the loaded skill's frontmatter summary visibly
#   - /compact dispatches the compact RPC — observed on the outgoing WS
#     frame (deterministic; the provider round-trip is NOT required for the
#     assertion, so an empty session is fine and the lane stays fast)
#   - Command copy: real-browser click, keyboard Enter, and mobile ≥40×40
#     touch target; clipboard is the exact multiline redacted command
#     (never the hidden secret); success/failure flash then reset
#   - File Explorer: ordinary Files open at 1360/900/390, integrated Git
#     M/A/D/R tree + dir aggregate, code/markdown infinite scroll, right-click
#     delete of a tracked row (exactly one file_explorer_delete, no download —
#     the tree download affordance was removed; git-only rows are inert),
#     header » hide reclaiming Send, global Files reopen of exact
#     selection/expansion/split/scroll/paging cache, and Ask on the committed
#     clean Token.sol (files.ask): visible Ask opens the source dock, one
#     unique question sends file_conversation_open for the displayed session
#     plus the exact path and a UUID then file_conversation_prompt on that
#     id, a real source selection prompts the same id with that range, the
#     selection toolbar Ask focuses the dock and Blame sends file_explorer_blame
#     for that exact range against the committed Token.sol identity (files.selection), the
#     assistant text is the fixture model's actual steering reply, Stop
#     aborts that exact id, a second clean file keeps its own conversation
#     while the first history stays, and Back plus the Files title close
#     send no conversation close and restore the same owner
#     Dependency map on committed MapRoot.sol (files.map): the Map surface
#     renders an actual SVG with the selected root, the resolved
#     ./MapDep.sol neighbor, the external forge-std/Test.sol import, the
#     unresolved ./MissingMap.sol import, and the reverse MapCaller.sol
#     dependent; the node list opens the exact target through the owner;
#     a stale file switch paints no map for the wrong file.
#   - files.selection-cache-compaction: retain Ask lines 4–7 while real source
#     paging reaches cursor 40000; disclosure, prompt, and accepted context agree.

# The fixture workspace is a TEMP git repo (loopback only; never the real
# repository) with a committed baseline, a dirty tracked file carrying TWO
# separated diff hunks (additions + deletions, including a hostile
# `<script>`/`<img onerror>` literal line), a staged new tracked file, and
# an oversize (>4000 diff lines) file whose complete body the panel
# eagerly page-fetches per file, plus File Explorer fixtures (deleted
# gone.txt, renamed nested/old.txt → nested/renamed.txt, long notes.md, a
# committed-never-modified clean.txt root sibling for the status-column
# alignment contract, a committed-then-deleted nested
# deployments/sepolia/deploy.json whose directories exist ONLY in the change
# catalog (unavailable virtual dirs hidden while the catalog row stays
# visible), a committed CLEAN Solidity Token.sol Source-lexing fixture (the
# Explorer preview — a dirty row would route the tree click to the diff
# surface instead), and a SEPARATE dirty src/ReviewToken.sol Solidity
# fixture whose committed baseline differs from the working tree — the
# review panel's real .sol changed diff)
# and a project skill under `.rpi/skills/greet/SKILL.md`.
# The listener is launched with `--approve` so the project skill is trusted
# and resolvable by the stateless `skill` RPC.
#
# Browser driver: playwright via npm (ephemeral install in the scenario work
# dir) with a system Chrome/Chromium binary or playwright's bundled chromium.
# Missing node, a failed playwright install, or no usable Chromium FAILS the
# lane (exit 1, setup); assertion failures exit 2+.
#
# Evidence publication is failure-clean and set-atomic: the playwright
# driver STAGES ONLY (screenshots, coverage-assertions.json, the assertions
# manifest) into the ONE nonce-scoped staging dir the shell atomically
# claimed before the producer started. After the producer exits 0 with its
# STAGED marker, the shell runs the final gates FIRST — web_sanity_http
# plus the close/listener gate (stdin EOF must stop the listener) — and only
# then performs THE sole commit: it validates the exact staged set/ledger
# (manifest lane/nonce/artifacts schema, unique contained regular-file
# artifact names, nothing missing or extra, documented AND executed ledger
# arrays equal elementwise with every count exactly 1), commits with ONE
# stage-dir -> immutable run-dir rename (runs/web-commands-review-<nonce>/)
# plus ONE atomically-renamed current.json pointer, and ONLY THEN prints the
# lane's sole PASS line. EVERY step runs inside ONE rollback scope: any
# validation failure or fault removes the staging dir/new run/temp and
# preserves the prior pointer byte-identical; the invocation trap removes
# this attempt's staging set / run dir / pointer temp on ANY setup, producer,
# or final-gate failure.
#
# Usage: bash E2E.d/web/commands_review.sh [run|list]
set -euo pipefail
SCRIPT_DIR="$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
E2E_DIR="$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd -P)"
# shellcheck source=../lib/common.sh
. "$E2E_DIR/lib/common.sh"
# shellcheck source=lib/fixture.sh
. "$SCRIPT_DIR/lib/fixture.sh"

# Full-lane publication state: the secure-random lane nonce derives the
# staging dir, the reserved run destination, and the pointer temp file, all
# ATOMICALLY claimed BEFORE the producer starts. root/evidence are top-level
# so the EXIT/signal traps always see them. Ownership booleans gate every
# discard: cleanup removes ONLY the exact staging dir / run dir / pointer
# temp file this attempt created and still owns — never a prior pointer, a
# prior published run, or another attempt's temp files. Once the sole commit
# succeeded (lane_committed=1) the published run dir + pointer are never
# touched by the trap.
root="" evidence=""
lane_nonce="" staging="" staging_owned=0 run_owned=0
pointer_tmp="" pointer_tmp_owned=0
lane_pointer_before_file="" lane_committed=0

cr_lane_discard() {
    if [ "$staging_owned" = 1 ] && [ -n "$staging" ]; then
        rm -rf "$staging"
        staging_owned=0
    fi
    if [ "$run_owned" = 1 ] && [ -n "$lane_nonce" ] \
        && ! grep -Fq "\"nonce\": \"$lane_nonce\"" "$evidence/current.json" 2>/dev/null; then
        rm -rf "$evidence/runs/web-commands-review-$lane_nonce"
    fi
    run_owned=0
    if [ "$pointer_tmp_owned" = 1 ] && [ -n "$pointer_tmp" ]; then
        rm -f "$pointer_tmp"
        pointer_tmp_owned=0
    fi
}

# Shared teardown for EVERY exit path until the commit succeeds; idempotent.
# Under INJECT_FAIL the post-mortem proves the failed run removed its
# staging set/run/temp and left the production pointer byte-identical
# (FILE snapshot + cmp — never command substitution, which strips trailing
# newlines and mangles bytes); a verification failure escalates to exit 1.
on_cr_lane_exit() {
    if [ "$lane_committed" != 1 ]; then
        cr_lane_discard
        if [ "${INJECT_FAIL:-0}" = "1" ]; then
            if [ -f "$lane_pointer_before_file" ]; then
                if ! cmp -s "$evidence/current.json" "$lane_pointer_before_file"; then
                    printf '[e2e] ERROR: %s\n' "$SCENARIO: INJECT_FAIL: the captured production pointer was modified by the failed run" >&2
                    exit 1
                fi
            elif [ -e "$evidence/current.json" ]; then
                printf '[e2e] ERROR: %s\n' "$SCENARIO: INJECT_FAIL: the failed run created a production pointer that did not exist before" >&2
                exit 1
            fi
            if [ -n "$lane_nonce" ] && [ -e "$evidence/runs/web-commands-review-$lane_nonce" ]; then
                printf '[e2e] ERROR: %s\n' "$SCENARIO: INJECT_FAIL: this attempt's run directory survived failure cleanup" >&2
                exit 1
            fi
            if [ -n "$pointer_tmp" ] && [ -e "$pointer_tmp" ]; then
                printf '[e2e] ERROR: %s\n' "$SCENARIO: INJECT_FAIL: this attempt's temp pointer survived failure cleanup" >&2
                exit 1
            fi
            log "$SCENARIO: INJECT_FAIL self-check OK: staging/run/temp removed, production pointer and prior runs untouched"
        fi
    fi
    if [ -n "${root:-}" ]; then
        web_kill_mock "$root" || true
    fi
    cleanup_e2e
}

on_cr_lane_signal() {
    local status
    case "$1" in
        HUP) status=129 ;;
        INT) status=130 ;;
        *)   status=143 ;;
    esac
    trap '' HUP INT TERM
    trap - EXIT
    on_cr_lane_exit
    log "$SCENARIO: terminated by signal $1 (exit $status) — staged run discarded, prior pointer preserved"
    exit "$status"
}

# Collision-resistant lane nonce: 12 secure-random bytes as lowercase hex
# plus the shell PID — satisfies the strict filename token grammar the commit
# validator enforces.
cr_lane_nonce() {
    local rand_hex=""
    if command -v openssl >/dev/null 2>&1; then
        rand_hex="$(openssl rand -hex 12 2>/dev/null || true)"
    fi
    if [ -z "$rand_hex" ]; then
        rand_hex="$(od -An -tx1 -N12 /dev/urandom 2>/dev/null | tr -d ' \n')"
    fi
    [ -n "$rand_hex" ] || fail "$SCENARIO: cannot generate a secure random lane nonce"
    printf '%s-%s\n' "$rand_hex" "$$"
}

# ---------------------------------------------------------------------------
# Full-lane commit: the driver STAGED ONLY; this shell validates the exact
# staged set — manifest top-level keys EXACTLY lane/nonce/artifacts, manifest
# lane EXACTLY 'web-commands-review', nonce EXACTLY the commit nonce (a
# strict filename token), registered artifact names unique contained relative
# paths (never the reserved manifest filename), the coverage ledger named,
# the staging dir the EXACT nonce-scoped direct child of the evidence root,
# the run dir a normalized direct child of runs/, staged entries a
# regular-file-only set (lstat — symlinks rejected) with nothing missing or
# extra, and the ledger's documented AND executed arrays equal to the
# documented File Explorer ID order elementwise with every count exactly 1 —
# then commits ONE stage-dir -> run-dir rename plus ONE atomically-renamed
# current.json pointer. A failure BEFORE or DURING the commit removes the
# staging dir, the renamed run dir, and the pointer temp file, and preserves
# the previous pointer byte-identical — never an early PASS pointer, never
# an orphan run dir.
commit_cr_run() { # <staging> <nonce> <pointer_tmp>
    local lane_staging="$1" lane_nonce_commit="$2" lane_pointer_tmp="$3" lane_evidence
    lane_evidence="$(dirname "$lane_staging")"
    node - "$lane_staging" "$lane_nonce_commit" "$lane_evidence" "$lane_pointer_tmp" <<'NODE' || return 1
const fs = require('node:fs');
const path = require('node:path');
const [staging, nonce, evidenceDir, pointerTmp] = process.argv.slice(2);
const LANE = 'web-commands-review';
const MANIFEST = 'playwright-assertions.json';
const LEDGER = 'coverage-assertions.json';
const EXPECTED_IDS = [
  'files.solidity-preview',
  'files.boot',
  'files.desktop-open',
  'files.git-tree',
  'files.virtual-dirs-hidden',
  'files.tree-split',
  'files.code-scroll',
  'files.markdown-scroll',
  'files.preview-toggle',
  'files.download',
  'files.hide-reclaim',
  'files.reopen',
  'files.tablet',
  'files.ask',
  'files.selection',
  'files.map',
  'files.fullscreen',
  'files.mobile',
  'files.selection-cache-compaction',
  'files.zero-errors',
];
let renamed = false;
const runDirName = `runs/${LANE}-${nonce}`;
const runDir = path.join(evidenceDir, runDirName);
try {
  if (!/^[A-Za-z0-9][A-Za-z0-9._-]{0,127}$/.test(nonce)) {
    throw new Error(`nonce is not a strict filename token: ${JSON.stringify(nonce)}`);
  }
  if (path.dirname(path.resolve(staging)) !== path.resolve(evidenceDir)
    || path.basename(staging) !== `.staging-${LANE}-${nonce}`) {
    throw new Error(`staging dir is not the exact nonce-scoped direct child of the evidence dir: ${staging}`);
  }
  if (path.dirname(runDir) !== path.join(evidenceDir, 'runs')
    || path.basename(runDir) !== `${LANE}-${nonce}`
    || path.posix.normalize(runDirName) !== runDirName
    || runDirName.split('/').length !== 2 || runDirName.split('/')[0] !== 'runs') {
    throw new Error(`run dir is not a normalized, contained runs/ child: ${runDirName}`);
  }
  if (path.dirname(path.resolve(pointerTmp)) !== path.resolve(evidenceDir)
    || !/^\.current\.json\.[A-Za-z0-9][A-Za-z0-9._-]{0,127}\.tmp$/.test(path.basename(pointerTmp))) {
    throw new Error(`pointer temp file is not a nonce-scoped direct child of the evidence dir: ${pointerTmp}`);
  }
  if (!fs.existsSync(staging)) throw new Error(`staging dir missing: ${staging}`);
  let manifest;
  try {
    manifest = JSON.parse(fs.readFileSync(path.join(staging, MANIFEST), 'utf8'));
  } catch (error) {
    throw new Error(`staged manifest is missing or unparseable: ${error.message}`);
  }
  const manifestKeys = Object.keys(manifest).sort();
  if (manifestKeys.length !== 3
    || manifestKeys.some((key, index) => key !== ['artifacts', 'lane', 'nonce'][index])) {
    throw new Error(`manifest top-level keys are not exactly lane/nonce/artifacts: ${JSON.stringify(Object.keys(manifest))}`);
  }
  if (manifest.lane !== LANE) throw new Error(`manifest lane mismatch: ${JSON.stringify(manifest.lane)}`);
  if (manifest.nonce !== nonce) {
    throw new Error(`manifest nonce mismatch: ${JSON.stringify(manifest.nonce)} != ${JSON.stringify(nonce)}`);
  }
  const artifacts = manifest.artifacts;
  if (!Array.isArray(artifacts) || artifacts.length === 0) throw new Error('manifest carries no registered artifacts');
  if (new Set(artifacts).size !== artifacts.length) throw new Error('manifest artifact names are not unique');
  for (const name of artifacts) {
    if (typeof name !== 'string' || name === '' || name.startsWith('/') || name.includes('..')
      || name.includes('\\')) {
      throw new Error(`manifest artifact is not a contained relative path: ${JSON.stringify(name)}`);
    }
    if (name === MANIFEST) {
      throw new Error(`manifest artifact name is reserved: ${JSON.stringify(name)}`);
    }
  }
  if (!artifacts.includes(LEDGER)) {
    throw new Error(`manifest artifacts does not name the coverage ledger: ${LEDGER}`);
  }
  // Exact staged set: registered artifacts + the manifest, nothing missing,
  // nothing extra, regular files only (lstat — no symlink), no nested dirs.
  const declared = new Set(artifacts);
  declared.add(MANIFEST);
  const actual = new Set();
  for (const entry of fs.readdirSync(staging, { withFileTypes: true })) {
    if (!entry.isFile() || entry.isSymbolicLink()) {
      throw new Error(`staged set contains a non-regular entry: ${entry.name}`);
    }
    actual.add(entry.name);
  }
  const missing = [...declared].filter((name) => !actual.has(name));
  const extras = [...actual].filter((name) => !declared.has(name));
  if (missing.length || extras.length) {
    throw new Error(`staged set mismatch: missing=${JSON.stringify(missing)} extra=${JSON.stringify(extras)}`);
  }
  for (const name of declared) {
    const stats = fs.lstatSync(path.join(staging, name));
    if (stats.isSymbolicLink() || !stats.isFile()) {
      throw new Error(`staged artifact is not a regular file: ${name}`);
    }
  }
  let ledger;
  try {
    ledger = JSON.parse(fs.readFileSync(path.join(staging, LEDGER), 'utf8'));
  } catch (error) {
    throw new Error(`staged ledger is missing or unparseable: ${error.message}`);
  }
  const ledgerKeys = Object.keys(ledger).sort();
  if (ledgerKeys.length !== 3
    || ledgerKeys.some((key, index) => key !== ['counts', 'documented', 'executed'][index])) {
    throw new Error(`ledger top-level keys are not exactly documented/executed/counts: ${JSON.stringify(Object.keys(ledger))}`);
  }
  const arraysEqual = (a, b) => Array.isArray(a) && a.length === b.length && a.every((x, i) => x === b[i]);
  if (!arraysEqual(ledger.documented, EXPECTED_IDS)) {
    throw new Error(`ledger documented mismatch: ${JSON.stringify(ledger.documented)}`);
  }
  if (!arraysEqual(ledger.executed, EXPECTED_IDS)) {
    throw new Error(`ledger executed (actual execution order) mismatch: ${JSON.stringify(ledger.executed)}`);
  }
  const counts = ledger.counts;
  if (!counts || typeof counts !== 'object' || Array.isArray(counts)
    || Object.keys(counts).length !== EXPECTED_IDS.length
    || EXPECTED_IDS.some((id) => counts[id] !== 1)) {
    throw new Error(`ledger counts are not exactly-once for every documented id: ${JSON.stringify(ledger.counts)}`);
  }
  fs.mkdirSync(path.join(evidenceDir, 'runs'), { recursive: true });
  // The shell pre-claimed the run destination as a RESERVED EMPTY directory:
  // rename(2) replaces an empty target atomically; a non-empty target could
  // only be a foreign/corrupt reservation and fails the rename.
  fs.renameSync(staging, runDir);
  renamed = true;
  const pointerDoc = {
    lane: LANE,
    nonce,
    run: runDirName,
    manifest: MANIFEST,
    coverage: LEDGER,
    artifacts: [...declared].sort(),
  };
  fs.writeFileSync(pointerTmp, `${JSON.stringify(pointerDoc, null, 2)}\n`);
  const written = JSON.parse(fs.readFileSync(pointerTmp, 'utf8'));
  if (JSON.stringify(written) !== JSON.stringify(pointerDoc)
    || Object.keys(written).length !== 6 || written.run !== runDirName) {
    throw new Error('pointer document is not the exact lane/nonce/run/manifest/coverage/artifacts identity');
  }
  fs.renameSync(pointerTmp, path.join(evidenceDir, 'current.json'));
} catch (error) {
  // Pre-pointer-commit failure removes THIS attempt's staging dir, renamed
  // run dir, and pointer temp file; the previous pointer (if any) keeps
  // naming the previous complete publication.
  fs.rmSync(staging, { recursive: true, force: true });
  if (renamed) fs.rmSync(runDir, { recursive: true, force: true });
  fs.rmSync(pointerTmp, { force: true });
  console.error(`${LANE}: publication commit FAILED: ${error && error.message ? error.message : error}`);
  process.exit(1);
}
NODE
}

SCENARIO="web-commands-review"
TOKEN="web-commands-review-e2e-token-$$-$(date +%s)"
# Fixture file + skill values the playwright half asserts against (kept in
# lockstep with commands_review_test.mjs).
DIRTY_FILE="greet.txt"
ADDED_FILE="added.txt"
BIG_FILE="big.txt"
NESTED_FILE="nested/deep.txt"
DELETED_FILE="gone.txt"
RENAMED_OLD="nested/old.txt"
RENAMED_FILE="nested/renamed.txt"
MARKDOWN_FILE="notes.md"
SELECTION_WINDOW_FILE="selection-window.txt"
HTML_FILE="page.html"
PREVIEW_MD_FILE="readme.md"
CLEAN_FILE="clean.txt"
SOL_FILE="Token.sol"
SOL_REVIEW_FILE="src/ReviewToken.sol"
SEPOLIA_FILE="deployments/sepolia/deploy.json"
SKILL_NAME="greet"
SOL_MAP_ROOT_FILE="MapRoot.sol"
SOL_MAP_DEP_FILE="MapDep.sol"
SOL_MAP_CALLER_FILE="MapCaller.sol"
SKILL_DESC="Greet skill for E2E"

# ---------------------------------------------------------------------------
# spawn `rpi --listen` with --approve so the project skill is trusted. This
# mirrors core.sh's self-contained spawn (web_spawn_rpi has no --approve knob
# and the standalone lanes must not edit the shared fixture helper to add one).
# $1 root  $2 evidence  $3 mock port. -> sets RPI_PID via register_pid.
# ---------------------------------------------------------------------------
spawn_rpi() {
    local root="$1" evidence="$2" port="$3"
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
            <"$stdin_path" 9>&- >"$evidence/rpi.stdout" 2>"$evidence/rpi.stderr"
    ) &
    RPI_PID=$!
    register_pid "$RPI_PID"
}

# ---------------------------------------------------------------------------
main() {
    case "${1:-run}" in
        list|--list|--dry-run)
            printf '%s\n' \
                'web-commands-review - composer command button left of textarea, command picker lists /compact /skill /code-review, /code-review draft + no auto-submit, Enter opens real review panel (HEAD→working tree + dirty file + changed lines), literal /code-review origin/master single-ref open (seeded local remote-tracking ref; exact from=origin/master includeWorkingTree=true no-to wire + origin/master→working tree label + fullscreen shell desktop/mobile + close restore), explicit hunk selection + file filter + keyboard nav + truncated banner + Enter comment/stream/queued-comment/abort (Shift+Enter newline, composing Enter never submits) + inline close confirm + owner-stamped Close panel + Escape close + mobile Files/Diff/Thread tabs, TUI/Web parity (M/A compact glyphs, basename-only rows with full path/state in data/title/aria, true-clean-sibling status-column alignment + no filename/chip overlap + own hit targets + chip opens the exact file diff desktop + mobile 44px, no rail overflow desktop + mobile), two separated hunks with second-hunk thread ownership + per-hunk drafts A/B + file switch clears selection, hostile diff/comment stays literal (no dialog/script side effect), 1.5s snapshot polling observed and stops on close, session switch sends stamped code_review_close + no stale rev args, /skill <fixture> visible summary, /compact outgoing-WS dispatch, command-copy click/keyboard/mobile + exact redacted payload, File Explorer 1360/900/390 Git M/A/D/R tree + infinite scroll + hide reclaim + reopen (playwright, hard gate)'
            return 0
            ;;
        run|all) ;;
        *) fail "usage: $0 [run|list]" ;;
    esac

    require_rpi
    web_require_browser
    require_cmd git

    local port url
    root="$(scenario_workspace "$SCENARIO")"
    evidence="$EVIDENCE_ROOT/$SCENARIO"
    # Traps FIRST: from the first claim onward, every exit path — EXIT,
    # HUP/INT/TERM — funnels into the ownership-tracked teardown.
    trap on_cr_lane_exit EXIT
    trap 'on_cr_lane_signal HUP' HUP
    trap 'on_cr_lane_signal INT' INT
    trap 'on_cr_lane_signal TERM' TERM
    lane_nonce="$(cr_lane_nonce)"
    # ATOMIC claims BEFORE anything spawns: the nonce-scoped staging dir
    # (plain no-clobber mkdir), the RESERVED EMPTY run destination, and the
    # O_EXCL pointer temp file. The commit later replaces the reservation
    # and the temp atomically, and cleanup owns exactly these three paths.
    mkdir -p "$evidence/runs"
    staging="$evidence/.staging-web-commands-review-$lane_nonce"
    mkdir "$staging" \
        || fail "$SCENARIO: atomic staging claim failed for nonce $lane_nonce (existing staging dir rejected)"
    staging_owned=1
    mkdir "$evidence/runs/web-commands-review-$lane_nonce" \
        || fail "$SCENARIO: atomic run-destination claim failed for nonce $lane_nonce (collision rejected)"
    run_owned=1
    pointer_tmp="$evidence/.current.json.$lane_nonce.tmp"
    node -e 'const fs = require("node:fs"); fs.closeSync(fs.openSync(process.argv[1], "wx"));' \
        "$pointer_tmp" 2>/dev/null \
        || fail "$SCENARIO: atomic pointer-temp claim failed for nonce $lane_nonce (existing temp file rejected)"
    pointer_tmp_owned=1
    # Byte-EXACT snapshot of the production pointer BEFORE this attempt, via
    # file copy (empty-file marker absent when no pointer exists yet): every
    # failure path can compare with cmp -s, never command substitution.
    lane_pointer_before_file="$root/pointer.before"
    if [ -f "$evidence/current.json" ]; then
        cp "$evidence/current.json" "$lane_pointer_before_file"
    else
        rm -f "$lane_pointer_before_file"
    fi
    printf '%s\n' "$TOKEN" >"$root/token"

    port="$(web_start_mock "$root" "$evidence")"

    # Seed a TEMP git repo (loopback fixture only; never the real repo) with a
    # committed baseline, then a dirty tracked file carrying TWO separated
    # diff hunks (including a hostile literal line), a staged new file, and
    # an oversize file so HEAD→working-tree code review has real content.
    git -C "$root/workspace" init -q
    git -C "$root/workspace" config user.email "web-e2e@example.com"
    git -C "$root/workspace" config user.name "Web E2E"
    git -C "$root/workspace" config commit.gpgsign false
    # Baseline: 18 lines with the two change regions >6 untouched lines apart
    # (git's default 3-line context), so the dirty tree produces TWO separated
    # hunks — @@ -1,6 +1,7 @@ (beta→BETA + insert delta) and
    # @@ -13,6 +14,7 @@ (kappa→KAPPA + hostile insert) — never one merged hunk.
    cat >"$root/workspace/$DIRTY_FILE" <<'EOF'
alpha
beta
gamma
epsilon
zeta
eta
theta
iota
one
two
three
four
five
six
seven
kappa
lambda
mu
EOF
    # Nested fixture file so the changed-file rail renders a collapsible
    # directory tree (nested/ -> deep.txt), exercised by the tree assertions.
    mkdir -p "$root/workspace/nested"
    cat >"$root/workspace/nested/deep.txt" <<'EOF'
deep-one
deep-two
deep-three
EOF
    git -C "$root/workspace" add -- "$DIRTY_FILE" nested/deep.txt
    git -C "$root/workspace" -c commit.gpgsign=false commit -q -m baseline
    # Dirty working tree, two SEPARATED hunks. The second hunk inserts a
    # hostile line (`<script>` + `<img onerror>`) that the diff pipeline must
    # treat as opaque text: the panel renders it literally (React text, never
    # innerHTML) and it must create no element and run no script.
    cat >"$root/workspace/$DIRTY_FILE" <<'EOF'
alpha
BETA
gamma
delta
epsilon
zeta
eta
theta
iota
one
two
three
four
five
six
seven
KAPPA
<script>window.__crPwned=1</script><img src=x onerror=window.__crPwned=2>
lambda
mu
EOF
    # Staged new tracked file (HEAD→working-tree review shows staged + unstaged).
    printf 'new file content\n' >"$root/workspace/$ADDED_FILE"
    git -C "$root/workspace" add -- "$ADDED_FILE"
    # Dirty the nested fixture file (deep-two -> DEEP-TWO) so the tree has a
    # real nested directory node with one changed file beneath it.
    cat >"$root/workspace/nested/deep.txt" <<'EOF'
deep-one
DEEP-TWO
deep-three
EOF
    # Oversize file: > MAX_FILE_RENDER_LINES (4000) rendered diff lines with
    # a COMPLETE body (well under the 2 MiB snapshot cap and every per-file
    # cap, so bodyState=complete): the panel eagerly fetches the file's full
    # backend page chain and real near-bottom scroll grows the visible
    # window 4000 → 8000 → 8200 with no truncated token, banner, bounded
    # status, or Load control. 4100 changed lines => ~8200 diff lines.
    BIG_LINES=4100
    for i in $(seq 1 "$BIG_LINES"); do printf 'base-line-%05d\n' "$i"; done >"$root/workspace/$BIG_FILE"
    git -C "$root/workspace" add -- "$BIG_FILE"
    # Pathspec-scoped commit: added.txt must stay staged-but-uncommitted so
    # HEAD→working-tree review still shows it as a new file.
    git -C "$root/workspace" -c commit.gpgsign=false commit -q -m "big baseline" -- "$BIG_FILE"
    for i in $(seq 1 "$BIG_LINES"); do printf 'changed-line-%05d\n' "$i"; done >"$root/workspace/$BIG_FILE"

    # File Explorer fixtures (loopback only): a committed-then-deleted path
    # (virtual D row), a committed-then-renamed nested path (virtual R row
    # under nested/), and a long markdown file for infinite-scroll paging.
    printf 'gone\n' >"$root/workspace/$DELETED_FILE"
    # A committed, tracked root file that is NEVER modified/staged/untracked
    # afterwards: the status-column alignment contract compares the changed
    # row against this TRUE clean sibling (a chip-carrying dirty sibling
    # cannot detect chip-induced indent/overlap — the P1 false-pass guard).
    printf 'clean\n' >"$root/workspace/$CLEAN_FILE"
    printf 'old\n' >"$root/workspace/$RENAMED_OLD"
    MD_LINES=520
    {
        printf '# File Explorer markdown fixture\n\n'
        for i in $(seq 1 "$MD_LINES"); do
            printf 'markdown-line-%04d this is a long wrapping sentence that exceeds eighty characters so wrap does not invent ghost line numbers.\n' "$i"
        done
    } >"$root/workspace/$MARKDOWN_FILE"
    for ((i = 0; i < 40400; i++)); do printf 'x\n'; done >"$root/workspace/$SELECTION_WINDOW_FILE"
    printf '<h1>File Explorer HTML fixture</h1>\n<p>preview body</p>\n<script>window.parent.__fePwned=1</script>\n' >"$root/workspace/$HTML_FILE"
    printf '# Preview heading\n\n**bold** preview fixture\n\n`notes.md:40`\n' >"$root/workspace/$PREVIEW_MD_FILE"
    # A committed-then-deleted NESTED path: deployments/sepolia/deploy.json is
    # removed from the working tree (git rm below), so deployments/ and
    # deployments/sepolia/ exist ONLY in the change catalog — no disk listing
    # of either directory can ever succeed. The explorer must keep the
    # changed-file catalog row visible (virtual git-only D) while the
    # unavailable virtual directories stay hidden (no expandable dir row, no
    # directory-unavailable error/Retry, no wire listing).
    mkdir -p "$root/workspace/deployments/sepolia"
    printf '{\n  "name": "e2e-sepolia-deploy"\n}\n' >"$root/workspace/$SEPOLIA_FILE"
    # A committed CLEAN Solidity source file for the Explorer: the Source
    # surface must lex it with a real Solidity grammar (specific keyword
    # tokens over the intact source), never a plain unhighlighted render.
    # Kept in lockstep with SOLIDITY_SOURCE_LINES in commands_review_test.mjs
    # and NEVER modified afterwards — a dirty row would route the explorer
    # tree click to the diff surface instead of the Source preview.
    cat >"$root/workspace/$SOL_FILE" <<'EOF'
// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

contract TokenStore {
    uint256 private totalSupply;

    function mint(uint256 amount) public {
        totalSupply += amount;
    }
}
EOF
    # A SEPARATE dirty Solidity fixture for the code-review panel: the
    # committed baseline below is a DIFFERENT (older) revision, and the
    # working tree is moved to the current version right after the commit, so
    # the review rail carries a real dirty .sol M row while the Explorer
    # Token.sol fixture above stays clean.
    mkdir -p "$root/workspace/$(dirname -- "$SOL_REVIEW_FILE")"
    cat >"$root/workspace/$SOL_REVIEW_FILE" <<'EOF'
// SPDX-License-Identifier: MIT
pragma solidity ^0.8.19;

contract OldToken {
    uint256 private supply;

    function burn(uint256 amount) public {
        supply -= amount;
    }
}
EOF
    # Dependency-map fixtures (files.map): three CLEAN committed Solidity files
    # exercising the real extractor. MapRoot imports ./MapDep.sol (resolved
    # neighbor), forge-std/Test.sol (external), and ./MissingMap.sol
    # (unresolved: no supported relative candidate exists in the scan).
    # MapCaller imports ./MapRoot.sol, the reverse dependent. Each contract
    # carries a natspec doc immediately before it, so the map's purpose text
    # is verbatim source documentation — never generated. The committed
    # Token.sol Source-lexing fixture above is NEVER modified.
    cat >"$root/workspace/$SOL_MAP_ROOT_FILE" <<'EOF'
// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

import "./MapDep.sol";
import "forge-std/Test.sol";
import "./MissingMap.sol";

/// @title Root ledger contract for the map fixture.
contract MapRootLedger {
    uint256 public stored;

    function store(uint256 amount) public {
        stored += amount;
    }
}
EOF
    cat >"$root/workspace/$SOL_MAP_DEP_FILE" <<'EOF'
// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

/// @title Dependency contract for the map fixture.
contract MapDepVault {
    uint256 public reserve;
}
EOF
    cat >"$root/workspace/$SOL_MAP_CALLER_FILE" <<'EOF'
// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

import "./MapRoot.sol";

/// @title Caller contract for the map fixture.
contract MapCallerGate {
    MapRootLedger public ledger;
}
EOF
    git -C "$root/workspace" add -- "$DELETED_FILE" "$RENAMED_OLD" "$MARKDOWN_FILE" "$SELECTION_WINDOW_FILE" "$HTML_FILE" "$PREVIEW_MD_FILE" "$CLEAN_FILE" "$SEPOLIA_FILE" "$SOL_FILE" "$SOL_REVIEW_FILE" "$SOL_MAP_ROOT_FILE" "$SOL_MAP_DEP_FILE" "$SOL_MAP_CALLER_FILE"
    git -C "$root/workspace" -c commit.gpgsign=false commit -q -m "files explorer fixtures" -- "$DELETED_FILE" "$RENAMED_OLD" "$MARKDOWN_FILE" "$SELECTION_WINDOW_FILE" "$HTML_FILE" "$PREVIEW_MD_FILE" "$CLEAN_FILE" "$SEPOLIA_FILE" "$SOL_FILE" "$SOL_REVIEW_FILE" "$SOL_MAP_ROOT_FILE" "$SOL_MAP_DEP_FILE" "$SOL_MAP_CALLER_FILE"
    SOL_COMMIT="$(git -C "$root/workspace" log -1 --format=%H -- "$SOL_FILE")"
    SOL_COMMIT_TIME="$(git -C "$root/workspace" log -1 --format=%at -- "$SOL_FILE")"
    # src/ReviewToken.sol's WORKING TREE is the current version (the review
    # diff's + side): the committed baseline above differs, so the review
    # panel renders a real dirty .sol changed diff while Token.sol — the
    # Explorer Source expectation — stays clean.
    cat >"$root/workspace/$SOL_REVIEW_FILE" <<'EOF'
// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

contract TokenStore {
    uint256 private totalSupply;

    function mint(uint256 amount) public {
        totalSupply += amount;
    }
}
EOF
    # refs/remotes/origin/master LOCALLY (git update-ref — no remote URL is
    # configured and no network is touched) pointing at the final fixture
    # commit. The working tree is dirty relative to it (added.txt staged,
    # greet.txt/big.txt/src/ReviewToken.sol modified, gone.txt deleted, rename), so the
    # literal user command `/code-review origin/master` is a real non-empty
    # <base>→working-tree review.
    git -C "$root/workspace" update-ref refs/remotes/origin/master HEAD
    git -C "$root/workspace" rev-parse --verify -q refs/remotes/origin/master >/dev/null \
        || fail "$SCENARIO: failed to seed refs/remotes/origin/master in the fixture repo"
    git -C "$root/workspace" rm -q -- "$DELETED_FILE"
    git -C "$root/workspace" rm -q -- "$SEPOLIA_FILE"
    git -C "$root/workspace" mv -- "$RENAMED_OLD" "$RENAMED_FILE"

    # Fixture project skill so /skill <name> resolves to a loaded frontmatter
    # summary. --approve (spawn_rpi) trusts the project `.rpi` scope.
    mkdir -p "$root/workspace/.rpi/skills/$SKILL_NAME"
    cat >"$root/workspace/.rpi/skills/$SKILL_NAME/SKILL.md" <<EOF
---
name: $SKILL_NAME
description: $SKILL_DESC
---
# $SKILL_NAME

Deterministic web e2e fixture skill body.
EOF

    # Extra root files make the File Explorer tree vertically scrollable at
    # the 1360×900 acceptance viewport; S0 requires a real nonzero tree scroll.
    for i in $(seq 1 80); do
        printf 'tree filler %03d\n' "$i" >"$root/workspace/tree-filler-$(printf '%03d' "$i").txt"
    done

    spawn_rpi "$root" "$evidence" "$port"
    url="$(web_wait_for_listener "$evidence")"
    log "$SCENARIO: listener at $url"

    local pw_status=0 pw_log="$evidence/playwright.out"
    web_run_playwright "$url" "$evidence" "$root/playwright" \
        "$SCRIPT_DIR/commands_review_test.mjs" \
        RPI_DIRTY_FILE="$DIRTY_FILE" \
        RPI_ADDED_FILE="$ADDED_FILE" \
        RPI_BIG_FILE="$BIG_FILE" \
        RPI_NESTED_FILE="$NESTED_FILE" \
        RPI_DELETED_FILE="$DELETED_FILE" \
        RPI_RENAMED_FILE="$RENAMED_FILE" \
        RPI_RENAMED_OLD="$RENAMED_OLD" \
        RPI_MARKDOWN_FILE="$MARKDOWN_FILE" \
        RPI_SELECTION_WINDOW_FILE="$SELECTION_WINDOW_FILE" \
        RPI_HTML_FILE="$HTML_FILE" \
        RPI_PREVIEW_MD_FILE="$PREVIEW_MD_FILE" \
        RPI_SOL_FILE="$SOL_FILE" \
        RPI_SOL_COMMIT="$SOL_COMMIT" \
        RPI_SOL_COMMIT_TIME="$SOL_COMMIT_TIME" \
        RPI_SOL_REVIEW_FILE="$SOL_REVIEW_FILE" \
        RPI_SOL_MAP_ROOT_FILE="$SOL_MAP_ROOT_FILE" \
        RPI_SOL_MAP_DEP_FILE="$SOL_MAP_DEP_FILE" \
        RPI_SOL_MAP_CALLER_FILE="$SOL_MAP_CALLER_FILE" \
        RPI_SEPOLIA_DELETED_FILE="$SEPOLIA_FILE" \
        RPI_CLEAN_FILE="$CLEAN_FILE" \
        RPI_SKILL_DESC="$SKILL_DESC" \
        RPI_LANE_NONCE="$lane_nonce" \
        RPI_STAGING="$staging" \
        RPI_FAILURE_EVIDENCE_DIR="$evidence" \
        INJECT_FAIL="${INJECT_FAIL:-0}" >"$pw_log" 2>&1 || pw_status=$?
    if [ "$pw_status" -eq 0 ]; then
        # Playwright STAGED ONLY — nothing is published yet. The pointer can
        # never name a run before the HTTP sanity and listener-close gates
        # below have passed.
        grep -Fqx "web-commands-review: STAGED $staging" "$pw_log" \
            || fail "$SCENARIO: playwright exited 0 without its STAGED marker — see $pw_log"
        printf 'playwright: command button + picker + /code-review panel + /skill summary + /compact dispatch staged\n' | tee "$root/playwright-summary.txt"
    elif [ "$pw_status" -eq 1 ]; then
        fail "$SCENARIO: playwright SETUP FAILED (node/chromium/npm unavailable)"
    else
        fail "$SCENARIO: playwright lane failed (exit $pw_status) — see $pw_log"
    fi

    # A listener is stdin-independent; SIGINT requests terminal host shutdown.
    web_sanity_http "$url"
    log "$SCENARIO: stopping the listener with SIGINT and verifying exit"
    exec 9>&- 9<&-
    kill -INT "$RPI_PID"
    local close_tries=0
    while [ "$close_tries" -lt 25 ] && kill -0 "${RPI_PID:-0}" 2>/dev/null; do
        sleep 0.2
        close_tries=$((close_tries + 1))
    done
    if kill -0 "${RPI_PID:-0}" 2>/dev/null; then
        fail "$SCENARIO: listener did not exit after SIGINT — publication commit withheld"
    fi
    wait "${RPI_PID:-0}" 2>/dev/null || true
    RPI_PID=""

    # Gate 3 — THE sole publication commit: exact staged-set/ledger
    # validation, ONE stage-dir -> immutable run-dir rename, ONE
    # atomically-renamed current.json pointer. HUP/INT/TERM are BLOCKED
    # across the commit AND the lane_committed transition: a signal can never
    # split the two-step publication. After the traps are restored there is
    # NO fallible action left except the PASS line and exit 0.
    local commit_ok=0
    trap '' HUP INT TERM
    commit_cr_run "$staging" "$lane_nonce" "$pointer_tmp" && commit_ok=1
    if [ "$commit_ok" = 1 ]; then
        lane_committed=1
    fi
    trap 'on_cr_lane_signal HUP' HUP
    trap 'on_cr_lane_signal INT' INT
    trap 'on_cr_lane_signal TERM' TERM
    [ "$commit_ok" = 1 ] \
        || fail "$SCENARIO: publication commit failed — staging/run/temp removed, prior pointer preserved"
    log "$SCENARIO: published $evidence/runs/web-commands-review-$lane_nonce via $evidence/current.json"
    printf '%s passed\nevidence=%s\n' "$SCENARIO" "$EVIDENCE_ROOT/$SCENARIO"
}

main "$@"